//! Chain verification and the single-use claim of a verified credential.
//!
//! [`MppService::verify_payment`] does the chain work and claims nothing; it can be repeated
//! for one credential, and its answer is a [`PaidCredential`]: proof, not yet spent, that the
//! credential's challenge was paid. [`MppService::claim`] spends it atomically, and only
//! once the request is certain to reach the handler. Splitting the two keeps an unpaid
//! credential from reaching the service key (verification comes first) and a paid one from
//! being consumed by a request a later layer refuses (the claim comes last).

use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use super::{MppService, VerifiedCredential};
use crate::mpp::error::{
    repository_error_kind, ConsumedReason, MppError, UnavailableReason, VerifyFailure,
};
use crate::mpp::ports::{ClaimLease, ClaimOutcome, RpcError, RpcReceipt, ServeOutcome};
use crate::mpp::protocol::{self, PaymentPayload};
use crate::mpp::tempo::{self, Expected, ReceiptVerdict};
use crate::mpp::transaction::{self, PaymentTerms};

/// A credential whose payment was seen on chain, bound to its challenge: the request may be
/// served once it is claimed. Cloneable and re-presentable: nothing is stored until
/// [`MppService::claim`].
#[derive(Clone, PartialEq, Eq)]
pub struct PaidCredential {
    challenge_id: String,
    /// The real transaction hash, `0x…`, for the `Payment-Receipt`.
    reference: String,
    /// `tempo:` and a keyed digest of the transaction hash: the claim-store key.
    payment_key: String,
}

#[cfg(any(test, feature = "test-mocks"))]
impl PaidCredential {
    /// The claim-store key. For assertions against a claim store.
    pub fn payment_key(&self) -> &str {
        &self.payment_key
    }
}

impl fmt::Debug for PaidCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PaidCredential { .. }")
    }
}

/// A payment that is claimed: the request may be served.
#[derive(Clone, PartialEq, Eq)]
pub struct SettledPayment {
    paid: PaidCredential,
    /// This attempt's claim: only it may be finished.
    lease: ClaimLease,
}

impl SettledPayment {
    /// `Payment-Receipt` header value for this payment.
    pub fn receipt_header(&self, at: DateTime<Utc>) -> Result<String, MppError> {
        protocol::format_receipt(&self.paid.reference, at).map_err(|_| MppError::Config)
    }
}

#[cfg(any(test, feature = "test-mocks"))]
impl SettledPayment {
    /// The claim-store key. For assertions against a claim store.
    pub fn payment_key(&self) -> &str {
        &self.paid.payment_key
    }
}

impl fmt::Debug for SettledPayment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SettledPayment { .. }")
    }
}

impl MppService {
    /// Verify on chain that `verified` paid its challenge. Claims nothing: the same
    /// credential may be verified again, and a payment found here that no request ever
    /// claims is simply not spent.
    ///
    /// Errors: a revert, a missing or wrong transfer, or a transaction that does not pay the
    /// challenge is [`MppError::VerificationFailed`] (a new payment can fix it);
    /// [`MppError::ReceiptNotFound`] and [`MppError::Unavailable`] are retries of the same
    /// credential; [`MppError::Consumed`] with `MemoNotBound` means funds moved for another
    /// challenge and paying again would not help.
    pub async fn verify_payment(
        &self,
        verified: &VerifiedCredential,
    ) -> Result<PaidCredential, MppError> {
        self.verify_payment_at(Utc::now(), verified).await
    }

    /// [`Self::verify_payment`] with an explicit clock (the expiry of a `transaction`
    /// payload's broadcast window is judged against `now`).
    pub async fn verify_payment_at(
        &self,
        now: DateTime<Utc>,
        verified: &VerifiedCredential,
    ) -> Result<PaidCredential, MppError> {
        // A credential whose payment is already spent is refused here, before any RPC call
        // and before the service key's rate limit, so replaying it costs nothing. The payment
        // key is known up front: the hash of a `hash` payload, `keccak256` of a transaction.
        let tx_hash = match &verified.payload {
            PaymentPayload::Hash(hash) => *hash,
            PaymentPayload::Transaction(raw) => tempo::transaction_hash(raw),
        };
        let payment_key =
            protocol::payment_key(&self.secret_key, &tx_hash).map_err(|_| MppError::Config)?;
        match self
            .repo
            .is_spent(&payment_key, &verified.challenge_id)
            .await
        {
            Ok(false) => {}
            Ok(true) => return Err(MppError::Consumed(ConsumedReason::AlreadyClaimed)),
            Err(error) => {
                tracing::error!(
                    outcome = "store_unavailable",
                    repository_error = repository_error_kind(&error),
                    "MPP spent-payment check failed"
                );
                return Err(MppError::Unavailable(UnavailableReason::Store));
            }
        }

        // One deadline for the whole phase: the broadcast, its recovery lookups and their
        // spacing, and for a late redemption the block-time lookup, however the node behaves.
        let late = verified.expires_at <= now;
        let chain_phase = async {
            let receipt = self.receipt_for(now, verified).await?;
            if late {
                self.mined_by_expiry(&receipt, verified.expires_at).await?;
            }
            Ok::<RpcReceipt, MppError>(receipt)
        };
        let receipt = match tokio::time::timeout(self.receipt_deadline, chain_phase).await {
            Ok(receipt) => receipt?,
            Err(_) => {
                tracing::warn!(outcome = "rpc_deadline", "MPP receipt phase timed out");
                return Err(MppError::Unavailable(UnavailableReason::Rpc));
            }
        };
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

        Ok(PaidCredential {
            challenge_id: verified.challenge_id.clone(),
            reference: format!("0x{}", hex::encode(receipt.tx_hash)),
            payment_key: protocol::payment_key(&self.secret_key, &receipt.tx_hash)
                .map_err(|_| MppError::Config)?,
        })
    }

    async fn receipt_for(
        &self,
        now: DateTime<Utc>,
        verified: &VerifiedCredential,
    ) -> Result<RpcReceipt, MppError> {
        let expired = verified.expires_at <= now;
        match &verified.payload {
            PaymentPayload::Hash(hash) => self.look_up(*hash, expired).await,
            PaymentPayload::Transaction(raw) => {
                // Nothing leaves this server unless it pays this challenge.
                let terms = PaymentTerms {
                    chain_id: self.chain_id,
                    currency: self.currency,
                    recipient: self.recipient,
                    amount: verified.amount,
                    realm: &self.realm,
                    challenge_id: &verified.challenge_id,
                };
                transaction::check_payment_transaction(raw, &terms).map_err(|mismatch| {
                    MppError::VerificationFailed(VerifyFailure::Transaction(mismatch))
                })?;
                let hash = tempo::transaction_hash(raw);
                // The window to broadcast is the challenge's lifetime. After it, a transaction
                // that was already mined can still be redeemed, by lookup only.
                if expired {
                    return self.look_up(hash, expired).await;
                }
                match self.rpc.send_raw_transaction_sync(raw).await {
                    Ok(receipt) if receipt.tx_hash == hash => Ok(receipt),
                    Ok(_) => Err(MppError::VerificationFailed(VerifyFailure::ReceiptMismatch)),
                    Err(send_error) => self.recover_receipt(hash, send_error).await,
                }
            }
        }
    }

    /// Look a payment up by its transaction hash. Not mined yet is a retry of the same
    /// credential while the challenge is live, and an expired challenge once it is not.
    async fn look_up(&self, hash: [u8; 32], expired: bool) -> Result<RpcReceipt, MppError> {
        match self.rpc.get_transaction_receipt(hash).await {
            Ok(Some(receipt)) if receipt.tx_hash == hash => Ok(receipt),
            Ok(Some(_)) => Err(MppError::VerificationFailed(VerifyFailure::ReceiptMismatch)),
            Ok(None) if expired => Err(MppError::Expired),
            Ok(None) => Err(MppError::ReceiptNotFound),
            Err(error) => Err(rpc_unavailable(error, "lookup")),
        }
    }

    /// A late redemption counts only for a payment mined by the challenge's expiry.
    async fn mined_by_expiry(
        &self,
        receipt: &RpcReceipt,
        expires_at: DateTime<Utc>,
    ) -> Result<(), MppError> {
        let mined_at = match self.rpc.get_block_timestamp(receipt.block_number).await {
            Ok(Some(mined_at)) => mined_at,
            Ok(None) => return Err(MppError::Unavailable(UnavailableReason::Rpc)),
            Err(error) => return Err(rpc_unavailable(error, "block")),
        };
        let deadline = u64::try_from(expires_at.timestamp()).map_err(|_| MppError::Config)?;
        if mined_at <= deadline {
            Ok(())
        } else {
            Err(MppError::Expired)
        }
    }

    /// The broadcast failed: the node may still hold the transaction (already known, or mined
    /// before its reply failed). Look the receipt up by `keccak256(raw)` a few times, giving
    /// up at the first lookup error. With no receipt, a node that refused the transaction
    /// means "not found" (the credential is retryable); one that did not answer means
    /// unavailable.
    async fn recover_receipt(
        &self,
        hash: [u8; 32],
        send_error: RpcError,
    ) -> Result<RpcReceipt, MppError> {
        for attempt in 0..self.recover_attempts {
            if attempt > 0 {
                tokio::time::sleep(self.recover_interval).await;
            }
            match self.rpc.get_transaction_receipt(hash).await {
                Ok(Some(receipt)) if receipt.tx_hash == hash => return Ok(receipt),
                Ok(Some(_)) => {
                    return Err(MppError::VerificationFailed(VerifyFailure::ReceiptMismatch))
                }
                Ok(None) => {}
                Err(error) => return Err(rpc_unavailable(error, "lookup")),
            }
        }
        match send_error {
            RpcError::Rejected => Err(MppError::ReceiptNotFound),
            RpcError::Unavailable | RpcError::InvalidResponse => {
                Err(rpc_unavailable(send_error, "broadcast"))
            }
        }
    }

    /// Spend a verified payment: one atomic step in the claim store. A payment already
    /// claimed (being served, served, or bound to another challenge) is
    /// [`MppError::Consumed`]; a store that did not answer is [`MppError::Unavailable`], and
    /// the same credential may be presented again.
    pub async fn claim(&self, paid: &PaidCredential) -> Result<SettledPayment, MppError> {
        match self.repo.claim(&paid.payment_key, &paid.challenge_id).await {
            Ok(ClaimOutcome::Claimed(lease)) => Ok(SettledPayment {
                paid: paid.clone(),
                lease,
            }),
            Ok(ClaimOutcome::AlreadyUsed) => {
                Err(MppError::Consumed(ConsumedReason::AlreadyClaimed))
            }
            Err(error) => {
                tracing::error!(
                    outcome = "store_unavailable",
                    repository_error = repository_error_kind(&error),
                    "MPP claim failed"
                );
                Err(MppError::Unavailable(UnavailableReason::Store))
            }
        }
    }

    /// Record how the paid request ended, retrying one store failure. Both attempts use the
    /// same lease, so a lost reply cannot finish a newer request. Persistent failure leaves
    /// the claim `serving` until maintenance reclaims it; log closed codes only.
    pub async fn finish(
        &self,
        payment: &SettledPayment,
        outcome: ServeOutcome,
    ) -> Result<(), MppError> {
        let mut result = self
            .repo
            .finish(&payment.paid.payment_key, payment.lease, outcome)
            .await;
        if result.is_err() {
            result = self
                .repo
                .finish(&payment.paid.payment_key, payment.lease, outcome)
                .await;
        }
        result.map_err(|error| {
            tracing::error!(
                outcome = "finish_failed",
                repository_error = repository_error_kind(&error),
                "MPP claim finish failed"
            );
            MppError::Unavailable(UnavailableReason::Store)
        })
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

/// Log a failed RPC call by closed code (never the node's text, a URL or a transaction).
fn rpc_unavailable(error: RpcError, call: &'static str) -> MppError {
    tracing::warn!(
        outcome = "rpc_unavailable",
        rpc_error = error.kind(),
        rpc_call = call,
        "MPP Tempo RPC call failed"
    );
    MppError::Unavailable(UnavailableReason::Rpc)
}

/// Holds a claimed payment while its request is served.
pub struct ServingGuard {
    service: Arc<MppService>,
    payment: Option<SettledPayment>,
}

#[cfg(any(test, feature = "test-mocks"))]
impl ServingGuard {
    pub fn payment(&self) -> Option<&SettledPayment> {
        self.payment.as_ref()
    }
}

impl ServingGuard {
    /// Record the outcome. A store failure is logged by [`MppService::finish`] and does not
    /// change the response the client gets.
    pub async fn finish(mut self, outcome: ServeOutcome) {
        if let Some(payment) = self.payment.take() {
            let _ = self.service.finish(&payment, outcome).await;
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
        // releases the claim. No runtime (process shutdown) leaves the row `serving` until
        // maintenance reclaims the abandoned attempt.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = service.finish(&payment, ServeOutcome::Served).await;
            });
        }
    }
}
