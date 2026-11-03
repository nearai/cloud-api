//! `MppService`: price a request, issue a challenge, verify a credential without state, check
//! its payment on chain, and claim it against the single-use ledger.
//!
//! The flow around one request, each step a separate call so the API layer can place the
//! expensive and the irreversible ones where they belong:
//!
//! 1. [`MppService::price_body`]: is the model priced, and are the request limits respected?
//! 2. [`MppService::challenge`]: a fresh HMAC-bound challenge for this body.
//! 3. [`MppService::verify`]: stateless checks of the echoed challenge and the payload shape.
//! 4. [`MppService::verify_payment`]: the chain work (receipt lookup, or the pre-checked
//!    broadcast) and the receipt verification. It claims nothing, so it can be repeated for
//!    the same credential and runs before the service key is used.
//! 5. [`MppService::claim`]: the atomic single-use claim, taken only once the request is
//!    certain to reach the handler.
//! 6. [`MppService::finish`]: record whether the paid request was served.
//!
//! Logs carry closed outcome codes only. No credential, header, transaction hash, payer
//! address, `source` DID, request body or secret is ever logged or put in an error.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use config::{MppModelPrice, MppSettings};

use super::error::{MppError, VerifyFailure};
use super::ports::{PaymentClaimRepository, TempoRpc};
use super::pricing::{LimitViolation, Param, Price, Pricing, RequestShape};
use super::protocol::{
    self, body_digest, opaque_for, opaque_route, parse_authorization, parse_source_did,
    BoundFields, Challenge, ChargeRequest, PaymentPayload, INTENT_CHARGE, METHOD,
};
use super::rpc::HttpTempoRpc;

mod settlement;

pub use settlement::{PaidCredential, ServingGuard, SettledPayment};

/// The only route MPP serves.
pub const CHAT_COMPLETIONS_ROUTE: &str = "/v1/chat/completions";

/// After a failed `eth_sendRawTransactionSync`, how often and how far apart the receipt is
/// looked up by hash (the node may have included the transaction before its reply failed).
/// How long after its challenge expired a credential may still redeem a payment that was
/// mined before the expiry. The expiry bounds when a client must *pay*; a payment already made
/// must not be stranded because the request was refused (rate limit, credits, an outage) until
/// the challenge ran out. After expiry nothing is broadcast: the payment is only looked up.
/// It must stay inside the claim retention (`retention.rs`), so a purged claim can never be
/// redeemed again.
pub const LATE_REDEMPTION: Duration = Duration::from_secs(12 * 60 * 60);

const DEFAULT_RECOVER_ATTEMPTS: u32 = 3;
const DEFAULT_RECOVER_INTERVAL: Duration = Duration::from_millis(400);

// ---------------------------------------------------------------------------------------
// Verified credentials
// ---------------------------------------------------------------------------------------

/// A credential that passed the stateless checks. Not yet shown to be paid: see
/// [`MppService::verify_payment`].
#[derive(Clone)]
pub struct VerifiedCredential {
    challenge_id: String,
    amount: u128,
    expires_at: DateTime<Utc>,
    payload: PaymentPayload,
    source: Option<[u8; 20]>,
}

#[cfg(test)]
impl VerifiedCredential {
    fn challenge_id(&self) -> &str {
        &self.challenge_id
    }

    fn amount(&self) -> u128 {
        self.amount
    }
}

impl fmt::Debug for VerifiedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VerifiedCredential { .. }")
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
    /// The whole receipt phase of `verify_payment` (broadcast, lookups, polls) ends here.
    receipt_deadline: Duration,
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
            // One call per RPC timeout, and a lookup phase after a failed broadcast: two
            // timeouts bound the phase however the node behaves.
            receipt_deadline: Duration::from_secs(settings.tempo.rpc_timeout_secs)
                .saturating_mul(2),
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

    /// Change the deadline of the whole receipt phase of [`Self::verify_payment`].
    pub fn with_receipt_deadline(mut self, deadline: Duration) -> Self {
        self.receipt_deadline = deadline;
        self
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

    /// Price `body` and issue its challenge: [`MppError::Limit`] when it cannot be paid for
    /// this way (an unpriced model is [`LimitViolation::ModelNotPriced`]).
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
        let late_redemption =
            chrono::Duration::from_std(LATE_REDEMPTION).map_err(|_| MppError::Config)?;
        if now >= expires_at + late_redemption {
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
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
