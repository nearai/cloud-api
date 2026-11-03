//! `MppService`: price a request, issue a challenge, verify a credential without state, settle
//! it against the chain and the single-use ledger.
//!
//! The flow around one request:
//!
//! 1. [`MppService::price_body`]: is the model priced, and are the request limits respected?
//! 2. [`MppService::challenge`]: a fresh HMAC-bound challenge for this body.
//! 3. [`MppService::verify`]: stateless checks of the echoed challenge and the payload shape.
//! 4. [`MppService::settle`]: chain verification, then the atomic claim.
//! 5. [`MppService::finish`]: record whether the paid request was served.
//!
//! Logs carry closed outcome codes only. No credential, header, transaction hash, payer
//! address, `source` DID, request body or secret is ever logged or put in an error.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use config::{MppModelPrice, MppSettings};

use super::ports::{
    ClaimOutcome, PaymentClaimRepository, RpcError, RpcReceipt, ServeOutcome, TempoRpc,
};
use super::pricing::{LimitViolation, Param, Price, Pricing, RequestShape};
use super::protocol::{
    self, body_digest, opaque_for, opaque_route, parse_authorization, parse_source_did,
    BoundFields, Challenge, ChargeRequest, PaymentPayload, ProblemCode, ProtocolError,
    INTENT_CHARGE, METHOD,
};
use super::rpc::HttpTempoRpc;
use super::tempo::{self, Expected, ReceiptVerdict};

/// The only route MPP serves.
pub const CHAT_COMPLETIONS_ROUTE: &str = "/v1/chat/completions";

/// After a failed `eth_sendRawTransactionSync`, how often and how far apart the receipt is
/// looked up by hash (the node may have included the transaction before its reply failed).
const DEFAULT_RECOVER_ATTEMPTS: u32 = 3;
const DEFAULT_RECOVER_INTERVAL: Duration = Duration::from_millis(400);

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/// Why chain verification failed in a way another payment (or a retry) can fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyFailure {
    Reverted,
    NoMatchingTransfer,
    ReceiptNotFound,
    ReceiptMismatch,
    SourceChain,
}

/// Why a verified payment can not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumedReason {
    AlreadyClaimed,
    /// The funds moved but not for this challenge: paying again would not help.
    MemoNotBound,
}

/// Which dependency was unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableReason {
    Rpc,
    Store,
}

/// Every way MPP processing can end other than success. Variants carry closed reasons only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MppError {
    #[error("malformed credential")]
    Malformed(&'static str),
    #[error("method unsupported")]
    MethodUnsupported,
    #[error("invalid challenge")]
    InvalidChallenge(&'static str),
    #[error("challenge expired")]
    Expired,
    #[error("payment verification failed")]
    VerificationFailed(VerifyFailure),
    #[error("payment already used")]
    Consumed(ConsumedReason),
    #[error("payment backend unavailable")]
    Unavailable(UnavailableReason),
    #[error("request limit violated")]
    Limit(LimitViolation),
    #[error("invalid MPP configuration")]
    Config,
}

impl MppError {
    /// Closed code for logs and metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "malformed_credential",
            Self::MethodUnsupported => "method_unsupported",
            Self::InvalidChallenge(_) => "invalid_challenge",
            Self::Expired => "payment_expired",
            Self::VerificationFailed(VerifyFailure::Reverted) => "tx_reverted",
            Self::VerificationFailed(VerifyFailure::NoMatchingTransfer) => "no_matching_transfer",
            Self::VerificationFailed(VerifyFailure::ReceiptNotFound) => "receipt_not_found",
            Self::VerificationFailed(VerifyFailure::ReceiptMismatch) => "receipt_mismatch",
            Self::VerificationFailed(VerifyFailure::SourceChain) => "source_chain_mismatch",
            Self::Consumed(ConsumedReason::AlreadyClaimed) => "already_claimed",
            Self::Consumed(ConsumedReason::MemoNotBound) => "memo_not_bound",
            Self::Unavailable(UnavailableReason::Rpc) => "rpc_unavailable",
            Self::Unavailable(UnavailableReason::Store) => "store_unavailable",
            Self::Limit(limit) => limit.code(),
            Self::Config => "config",
        }
    }

    /// The HTTP status to answer with.
    pub fn status(&self) -> u16 {
        match self {
            Self::MethodUnsupported | Self::Limit(_) => 400,
            Self::Malformed(_)
            | Self::InvalidChallenge(_)
            | Self::Expired
            | Self::VerificationFailed(_) => 402,
            Self::Consumed(_) => 403,
            Self::Unavailable(_) | Self::Config => 503,
        }
    }

    /// The RFC 9457 problem type, when the error is answered with one.
    pub fn problem(&self) -> Option<ProblemCode> {
        match self {
            Self::Malformed(_) => Some(ProblemCode::MalformedCredential),
            Self::MethodUnsupported => Some(ProblemCode::MethodUnsupported),
            Self::InvalidChallenge(_) => Some(ProblemCode::InvalidChallenge),
            Self::Expired => Some(ProblemCode::PaymentExpired),
            Self::VerificationFailed(_) | Self::Consumed(_) => {
                Some(ProblemCode::VerificationFailed)
            }
            Self::Unavailable(_) | Self::Limit(_) | Self::Config => None,
        }
    }

    /// Whether the response carries a fresh challenge (every 402).
    pub fn carries_challenge(&self) -> bool {
        self.status() == 402
    }

    /// A fixed, input-free sentence for the problem `detail`.
    pub fn detail(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "The Payment credential could not be read.",
            Self::MethodUnsupported => "This payment method is not supported.",
            Self::InvalidChallenge(_) => {
                "The challenge in the credential is not valid for this request."
            }
            Self::Expired => "The challenge has expired.",
            Self::VerificationFailed(VerifyFailure::ReceiptNotFound) => {
                "The transaction was not found or is not yet mined."
            }
            Self::VerificationFailed(_) => "Payment verification failed.",
            Self::Consumed(_) => "This payment has already been used.",
            Self::Unavailable(_) | Self::Config => {
                "Payment verification is temporarily unavailable."
            }
            Self::Limit(_) => "The request is not eligible for payment.",
        }
    }
}

impl From<ProtocolError> for MppError {
    fn from(error: ProtocolError) -> Self {
        match error {
            ProtocolError::Malformed(reason) => Self::Malformed(reason),
            ProtocolError::MethodUnsupported => Self::MethodUnsupported,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Verified and settled payments
// ---------------------------------------------------------------------------------------

/// A credential that passed the stateless checks. Not yet paid: see [`MppService::settle`].
#[derive(Clone)]
pub struct VerifiedCredential {
    challenge_id: String,
    amount: u128,
    expires_at: DateTime<Utc>,
    payload: PaymentPayload,
    source: Option<[u8; 20]>,
}

impl VerifiedCredential {
    pub fn challenge_id(&self) -> &str {
        &self.challenge_id
    }

    pub fn amount(&self) -> u128 {
        self.amount
    }
}

impl fmt::Debug for VerifiedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VerifiedCredential { .. }")
    }
}

/// A payment that is claimed: the request may be served.
#[derive(Clone, PartialEq, Eq)]
pub struct SettledPayment {
    payment_key: String,
    reference: String,
}

impl SettledPayment {
    /// `Payment-Receipt` header value for this payment.
    pub fn receipt_header(&self, at: DateTime<Utc>) -> Result<String, MppError> {
        protocol::format_receipt(&self.reference, at).map_err(|_| MppError::Config)
    }

    /// The claim key, `tempo:` and a 64-hex keyed digest. For the claim store only.
    pub fn payment_key(&self) -> &str {
        &self.payment_key
    }
}

impl fmt::Debug for SettledPayment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SettledPayment { .. }")
    }
}

// ---------------------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------------------

/// MPP for Tempo `charge` on `POST /v1/chat/completions`.
pub struct MppService {
    realm: String,
    secret_key: Vec<u8>,
    service_api_key: String,
    challenge_ttl: chrono::Duration,
    chain_id: u64,
    currency: [u8; 20],
    currency_hex: String,
    recipient: [u8; 20],
    recipient_hex: String,
    prices: BTreeMap<String, MppModelPrice>,
    /// The largest `max_request_bytes` of any priced model: a longer body is never parsed.
    largest_priced_body: usize,
    repo: Arc<dyn PaymentClaimRepository>,
    rpc: Arc<dyn TempoRpc>,
    recover_attempts: u32,
    recover_interval: Duration,
}

impl fmt::Debug for MppService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MppService")
            .field("realm", &self.realm)
            .field("chain_id", &self.chain_id)
            .field("models", &self.prices.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

fn address_hex(raw: &str) -> Result<([u8; 20], String), MppError> {
    let bytes = protocol::decode_prefixed_hex(raw, 20).map_err(|_| MppError::Config)?;
    let address: [u8; 20] = bytes.try_into().map_err(|_| MppError::Config)?;
    Ok((address, format!("0x{}", hex::encode(address))))
}

impl MppService {
    /// Build from validated settings with explicit ports (tests and harnesses inject fakes).
    pub fn new(
        settings: &MppSettings,
        repo: Arc<dyn PaymentClaimRepository>,
        rpc: Arc<dyn TempoRpc>,
    ) -> Result<Self, MppError> {
        if settings.secret_key.len() < config::MIN_MPP_SECRET_KEY_BYTES {
            return Err(MppError::Config);
        }
        let (currency, currency_hex) = address_hex(&settings.tempo.currency)?;
        let (recipient, recipient_hex) = address_hex(&settings.tempo.recipient)?;
        let ttl_secs = i64::try_from(settings.challenge_ttl_secs).map_err(|_| MppError::Config)?;
        Ok(Self {
            realm: settings.realm.clone(),
            secret_key: settings.secret_key.as_bytes().to_vec(),
            service_api_key: settings.service_api_key.clone(),
            challenge_ttl: chrono::Duration::seconds(ttl_secs),
            chain_id: settings.tempo.chain_id,
            currency,
            currency_hex,
            recipient,
            recipient_hex,
            prices: settings.model_prices.clone(),
            largest_priced_body: settings
                .model_prices
                .values()
                .map(|price| usize::try_from(price.max_request_bytes).unwrap_or(usize::MAX))
                .max()
                .unwrap_or(0),
            repo,
            rpc,
            recover_attempts: DEFAULT_RECOVER_ATTEMPTS,
            recover_interval: DEFAULT_RECOVER_INTERVAL,
        })
    }

    /// Build with the real Tempo JSON-RPC client from `settings.tempo`.
    pub fn from_settings(
        settings: &MppSettings,
        repo: Arc<dyn PaymentClaimRepository>,
    ) -> Result<Self, MppError> {
        let rpc = HttpTempoRpc::new(
            &settings.tempo.rpc_url,
            Duration::from_secs(settings.tempo.rpc_timeout_secs),
        )
        .map_err(|_| MppError::Config)?;
        Self::new(settings, repo, Arc::new(rpc))
    }

    /// Change the post-broadcast receipt lookup (attempts, spacing). Tests set it to zero.
    pub fn with_recover_poll(mut self, attempts: u32, interval: Duration) -> Self {
        self.recover_attempts = attempts;
        self.recover_interval = interval;
        self
    }

    pub fn realm(&self) -> &str {
        &self.realm
    }

    /// The `sk-` key MPP traffic is authenticated and metered under.
    pub fn service_api_key(&self) -> &str {
        &self.service_api_key
    }

    // ----- pricing -----

    /// Price a request: the model's flat price if every limit holds.
    pub fn price_for(&self, shape: &RequestShape, body_len: usize) -> Pricing {
        let Some(price) = self.prices.get(&shape.model) else {
            return Pricing::Unpriced;
        };
        let violation = if body_len as u64 > u64::from(price.max_request_bytes) {
            Some(LimitViolation::BodyTooLarge {
                cap: price.max_request_bytes,
            })
        } else if shape.n != Param::Absent && shape.n != Param::Value(1) {
            Some(LimitViolation::MultipleChoices)
        } else {
            match shape.requested_tokens() {
                Param::Absent => Some(LimitViolation::MaxTokensRequired {
                    cap: price.max_tokens,
                }),
                Param::Invalid => Some(LimitViolation::MaxTokensInvalid),
                Param::Value(tokens) if tokens > u64::from(price.max_tokens) => {
                    Some(LimitViolation::MaxTokensTooLarge {
                        cap: price.max_tokens,
                    })
                }
                Param::Value(_) => None,
            }
        };
        match violation {
            Some(violation) => Pricing::Violation(violation),
            None => Pricing::Priced(Price {
                amount: price.amount,
            }),
        }
    }

    /// Price a raw request body. An unparseable body is [`Pricing::Unpriced`], and so is one
    /// longer than any priced model accepts: it is not parsed at all, so a large body on the
    /// unauthenticated path costs nothing here.
    pub fn price_body(&self, body: &[u8]) -> Pricing {
        if body.len() > self.largest_priced_body {
            return Pricing::Unpriced;
        }
        match RequestShape::from_body(body) {
            Some(shape) => self.price_for(&shape, body.len()),
            None => Pricing::Unpriced,
        }
    }

    // ----- challenge -----

    /// A fresh challenge bound to `route`, this exact `body` and `price`.
    pub fn challenge(
        &self,
        route: &str,
        body: &[u8],
        price: &Price,
    ) -> Result<Challenge, MppError> {
        self.challenge_at(Utc::now(), route, body, price)
    }

    pub fn challenge_at(
        &self,
        now: DateTime<Utc>,
        route: &str,
        body: &[u8],
        price: &Price,
    ) -> Result<Challenge, MppError> {
        let request = ChargeRequest {
            amount: price.amount,
            currency: self.currency_hex.clone(),
            chain_id: self.chain_id,
            recipient: self.recipient_hex.clone(),
        }
        .to_b64()?;
        let nonce: [u8; protocol::OPAQUE_NONCE_BYTES] = rand::random();
        let opaque = opaque_for(route, &nonce)?;
        let digest = body_digest(body);
        // Seconds precision on the wire: truncate so the id binds the string the client sees.
        let expires = Utc
            .timestamp_opt((now + self.challenge_ttl).timestamp(), 0)
            .single()
            .ok_or(MppError::Config)?;
        let expires_string = protocol::format_timestamp(expires);
        let id = protocol::compute_challenge_id(
            &self.secret_key,
            &BoundFields {
                realm: &self.realm,
                method: METHOD,
                intent: INTENT_CHARGE,
                request: &request,
                expires: Some(&expires_string),
                digest: Some(&digest),
                opaque: Some(&opaque),
            },
        )?;
        Ok(Challenge {
            id,
            realm: self.realm.clone(),
            request,
            expires,
            digest,
            opaque,
        })
    }

    /// Price `body` and issue its challenge: [`MppError::NotPriced`] / [`MppError::Limit`]
    /// when it cannot be paid for this way.
    pub fn challenge_for_body(&self, route: &str, body: &[u8]) -> Result<Challenge, MppError> {
        match self.price_body(body) {
            Pricing::Priced(price) => self.challenge(route, body, &price),
            Pricing::Violation(violation) => Err(MppError::Limit(violation)),
            Pricing::Unpriced => Err(MppError::Limit(LimitViolation::ModelNotPriced)),
        }
    }

    // ----- verify (stateless) -----

    /// Decode and check an `Authorization: Payment …` value for this request.
    pub fn verify(
        &self,
        authorization: &str,
        route: &str,
        body: &[u8],
    ) -> Result<VerifiedCredential, MppError> {
        self.verify_at(Utc::now(), authorization, route, body)
    }

    pub fn verify_at(
        &self,
        now: DateTime<Utc>,
        authorization: &str,
        route: &str,
        body: &[u8],
    ) -> Result<VerifiedCredential, MppError> {
        let credential = parse_authorization(authorization)?;
        let echo = &credential.challenge;

        if echo.method != METHOD {
            return Err(MppError::MethodUnsupported);
        }
        if echo.intent != INTENT_CHARGE {
            return Err(MppError::InvalidChallenge("intent"));
        }
        if echo.realm != self.realm {
            return Err(MppError::InvalidChallenge("realm"));
        }
        // The id must be the HMAC of exactly these echoed values (constant-time).
        if !protocol::verify_challenge_id(&self.secret_key, &echo.id, &echo.bound_fields()) {
            return Err(MppError::InvalidChallenge("id"));
        }
        // From here on the values are ones this server issued: read them.
        let expires_at = echo
            .expires
            .as_deref()
            .and_then(|expires| DateTime::parse_from_rfc3339(expires).ok())
            .map(|expires| expires.with_timezone(&Utc))
            .ok_or(MppError::InvalidChallenge("expires"))?;
        if expires_at <= now {
            return Err(MppError::Expired);
        }
        let request = ChargeRequest::from_b64(&echo.request)
            .map_err(|_| MppError::InvalidChallenge("request"))?;
        if request.amount == 0
            || !request.currency.eq_ignore_ascii_case(&self.currency_hex)
            || !request.recipient.eq_ignore_ascii_case(&self.recipient_hex)
            || request.chain_id != self.chain_id
        {
            return Err(MppError::InvalidChallenge("request"));
        }
        if echo.digest.as_deref() != Some(body_digest(body).as_str()) {
            return Err(MppError::InvalidChallenge("digest"));
        }
        // The opaque value is ours (HMAC-checked above) and carries a nonce; only the route
        // it was issued for matters here.
        if echo.opaque.as_deref().and_then(opaque_route).as_deref() != Some(route) {
            return Err(MppError::InvalidChallenge("opaque"));
        }
        let source = match credential.source.as_deref() {
            None => None,
            Some(source) => {
                let (chain_id, address) = parse_source_did(source)?;
                if chain_id != self.chain_id {
                    return Err(MppError::VerificationFailed(VerifyFailure::SourceChain));
                }
                Some(address)
            }
        };
        Ok(VerifiedCredential {
            challenge_id: echo.id.clone(),
            amount: request.amount,
            expires_at,
            payload: credential.payload,
            source,
        })
    }

    // ----- settle -----

    /// Verify the payment on chain, then claim it. [`MppError::VerificationFailed`] is
    /// retryable with a new payment, [`MppError::Consumed`] is final, and
    /// [`MppError::Unavailable`] means the RPC or the store could not answer.
    pub async fn settle(&self, verified: &VerifiedCredential) -> Result<SettledPayment, MppError> {
        let receipt = self.receipt_for(verified).await?;
        let expected = Expected {
            currency: self.currency,
            recipient: self.recipient,
            amount: verified.amount,
            source: verified.source,
            realm: &self.realm,
            challenge_id: &verified.challenge_id,
        };
        tempo::verify_receipt(&receipt, &expected).map_err(|verdict| match verdict {
            ReceiptVerdict::Reverted => MppError::VerificationFailed(VerifyFailure::Reverted),
            ReceiptVerdict::NoMatchingTransfer => {
                MppError::VerificationFailed(VerifyFailure::NoMatchingTransfer)
            }
            ReceiptVerdict::MemoNotBound => MppError::Consumed(ConsumedReason::MemoNotBound),
        })?;

        let reference = format!("0x{}", hex::encode(receipt.tx_hash));
        let payment_key = protocol::payment_key(&self.secret_key, &receipt.tx_hash)
            .map_err(|_| MppError::Config)?;
        match self.repo.claim(&payment_key, &verified.challenge_id).await {
            Ok(ClaimOutcome::Claimed) => Ok(SettledPayment {
                payment_key,
                reference,
            }),
            Ok(ClaimOutcome::AlreadyUsed) => {
                Err(MppError::Consumed(ConsumedReason::AlreadyClaimed))
            }
            Err(_) => {
                tracing::error!(outcome = "store_unavailable", "MPP claim failed");
                Err(MppError::Unavailable(UnavailableReason::Store))
            }
        }
    }

    async fn receipt_for(&self, verified: &VerifiedCredential) -> Result<RpcReceipt, MppError> {
        match &verified.payload {
            PaymentPayload::Hash(hash) => match self.rpc.get_transaction_receipt(*hash).await {
                Ok(Some(receipt)) if receipt.tx_hash == *hash => Ok(receipt),
                Ok(Some(_)) => Err(MppError::VerificationFailed(VerifyFailure::ReceiptMismatch)),
                Ok(None) => Err(MppError::VerificationFailed(VerifyFailure::ReceiptNotFound)),
                Err(error) => Err(rpc_unavailable(error)),
            },
            PaymentPayload::Transaction(raw) => {
                // The window to broadcast is the challenge's lifetime.
                if verified.expires_at <= Utc::now() {
                    return Err(MppError::Expired);
                }
                match self.rpc.send_raw_transaction_sync(raw).await {
                    Ok(receipt) => Ok(receipt),
                    Err(send_error) => {
                        self.recover_receipt(tempo::transaction_hash(raw), send_error)
                            .await
                    }
                }
            }
        }
    }

    /// The broadcast failed: the node may still hold the transaction (already known, or mined
    /// before its reply failed). Look the receipt up by `keccak256(raw)` a few times. With no
    /// receipt, a node that refused the transaction means a retryable 402; an unreachable one
    /// means unavailable.
    async fn recover_receipt(
        &self,
        hash: [u8; 32],
        send_error: RpcError,
    ) -> Result<RpcReceipt, MppError> {
        let mut lookup_failed = false;
        for attempt in 0..self.recover_attempts {
            if attempt > 0 {
                tokio::time::sleep(self.recover_interval).await;
            }
            match self.rpc.get_transaction_receipt(hash).await {
                Ok(Some(receipt)) if receipt.tx_hash == hash => return Ok(receipt),
                Ok(Some(_)) => {
                    return Err(MppError::VerificationFailed(VerifyFailure::ReceiptMismatch))
                }
                Ok(None) => lookup_failed = false,
                Err(_) => lookup_failed = true,
            }
        }
        if lookup_failed || send_error != RpcError::Rejected {
            tracing::warn!(outcome = "rpc_unavailable", "MPP broadcast failed");
            return Err(MppError::Unavailable(UnavailableReason::Rpc));
        }
        Err(MppError::VerificationFailed(VerifyFailure::ReceiptNotFound))
    }

    // ----- finish -----

    /// Record how the paid request ended. An error leaves the claim `serving`; callers log it.
    pub async fn finish(
        &self,
        payment: &SettledPayment,
        outcome: ServeOutcome,
    ) -> Result<(), MppError> {
        self.repo
            .finish(&payment.payment_key, outcome)
            .await
            .map_err(|_| MppError::Unavailable(UnavailableReason::Store))
    }

    /// A guard that marks `payment` served if it is dropped before [`ServingGuard::finish`]
    /// (the client disconnected and the handler future was cancelled).
    pub fn serving(self: &Arc<Self>, payment: SettledPayment) -> ServingGuard {
        ServingGuard {
            service: self.clone(),
            payment: Some(payment),
        }
    }
}

fn rpc_unavailable(_error: RpcError) -> MppError {
    tracing::warn!(outcome = "rpc_unavailable", "MPP receipt lookup failed");
    MppError::Unavailable(UnavailableReason::Rpc)
}

/// Holds a claimed payment while its request is served.
pub struct ServingGuard {
    service: Arc<MppService>,
    payment: Option<SettledPayment>,
}

impl ServingGuard {
    pub fn payment(&self) -> Option<&SettledPayment> {
        self.payment.as_ref()
    }

    /// Record the outcome. Logs a closed code if the store fails.
    pub async fn finish(mut self, outcome: ServeOutcome) {
        if let Some(payment) = self.payment.take() {
            if self.service.finish(&payment, outcome).await.is_err() {
                tracing::error!(outcome = "finish_failed", "MPP claim finish failed");
            }
        }
    }
}

impl Drop for ServingGuard {
    fn drop(&mut self) {
        let Some(payment) = self.payment.take() else {
            return;
        };
        let service = self.service.clone();
        // Dropped means the client went away while its paid request ran (the work may already
        // be done). That counts as served: marking it unserved would let one payment buy a new
        // inference on every disconnect until the challenge expires. Only an error response
        // releases the claim. No runtime (process shutdown) leaves the row `serving`, which also
        // refuses reuse.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if service
                    .finish(&payment, ServeOutcome::Served)
                    .await
                    .is_err()
                {
                    tracing::error!(outcome = "finish_failed", "MPP claim finish failed");
                }
            });
        }
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
