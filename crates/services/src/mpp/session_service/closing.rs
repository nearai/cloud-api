//! Cooperative close and reconciliation of uncertain broadcasts.

use chrono::Utc;

use crate::mpp::abi::{self, ChannelDescriptor};
use crate::mpp::channel::voucher_signer;
use crate::mpp::eip712::close_authorization_digest;
use crate::mpp::error::{CloseReason, MppError, SessionMismatch, VoucherReason};
use crate::mpp::ports::{ChannelLifecycle, ChannelRecord, CloseTransition, PendingClose};
use crate::mpp::secp256k1::recover_address;
use crate::mpp::session_chain::{broadcast, build_payee_tx, BroadcastError, PayeeChain};
use crate::mpp::session_close_reconciliation::{reconcile_pending_close, CloseRecovery};
use crate::mpp::session_service::{
    receipt, store_error, ManagementReceipt, SessionCredential, SessionService,
};
use crate::mpp::session_wire::SessionPayload;

impl SessionService {
    pub async fn close(
        &self,
        credential: &SessionCredential,
    ) -> Result<ManagementReceipt, MppError> {
        let SessionPayload::Close {
            channel_id: id,
            descriptor,
            cumulative_amount,
            signature,
            close_signature,
        } = &credential.payload
        else {
            return Err(MppError::SessionMismatch(SessionMismatch::Action));
        };
        self.verify_signature(id, descriptor, *cumulative_amount, signature)?;
        self.check_close_authorization(id, descriptor, *cumulative_amount, close_signature)?;
        let _permit = self.management_permit()?;
        let mut record = self.get(id).await?;
        self.bind(&record, descriptor)?;
        if record.state == ChannelLifecycle::Closing {
            if let Some(receipt) = self.reconcile_close(credential, &record).await? {
                return Ok(receipt);
            }
            record = self.get(id).await?;
        }
        Self::accepting(&record)?;
        let record = self.refresh(record).await?;
        let capture = record.spent.max(record.settled_on_chain);
        if *cumulative_amount < capture {
            return Err(MppError::CloseRejected(CloseReason::BelowCapture));
        }
        if record.reserved != 0 {
            return Err(MppError::CloseRejected(CloseReason::Busy));
        }
        if *cumulative_amount < record.highest_cumulative {
            // A cooperative close can reduce unused authorization to confirmed spend or
            // an earlier snapshot's amount. Capture is already covered, and both the
            // voucher and close authorization must sign this exact final amount.
            if *cumulative_amount > record.deposit {
                return Err(MppError::VoucherRejected(VoucherReason::AboveDeposit));
            }
            self.verify_signature(id, descriptor, *cumulative_amount, signature)?;
        } else {
            self.check_voucher(
                &record,
                *cumulative_amount,
                signature,
                credential.min_voucher_delta,
                true,
            )?;
        }
        let calldata = abi::close_calldata(descriptor, *cumulative_amount, capture, signature)
            .map_err(|_| MppError::CloseRejected(CloseReason::InvalidSignature))?;
        let chain = PayeeChain {
            chain_id: self.config.chain_id,
            escrow: self.config.escrow,
            fee_token: self.config.currency,
        };
        let now = u64::try_from(Utc::now().timestamp()).map_err(|_| MppError::Config)?;
        let tx = build_payee_tx(
            chain,
            self.signer.as_ref(),
            self.rpc.as_ref(),
            calldata,
            now,
        )
        .await
        .map_err(|_| MppError::ChainTxFailed)?;
        let pending = PendingClose {
            tx_hash: tx.tx_hash,
            cumulative: *cumulative_amount,
            capture,
            valid_before: tx.valid_before,
        };
        let marked = match self
            .repo
            .mark_closing(id, pending, Utc::now())
            .await
            .map_err(store_error)?
        {
            CloseTransition::Marked(record) => record,
            CloseTransition::SettlementInFlight => {
                return Err(MppError::CloseRejected(CloseReason::Busy))
            }
            CloseTransition::NotOpen => return Err(MppError::ChannelClosing),
            CloseTransition::Missing => return Err(MppError::ChannelNotFound),
        };
        // The atomic transition fences further reservations. Recheck all capture-sensitive
        // state returned by that transition; another instance may have charged during build.
        if marked.reserved != 0
            || marked.spent.max(marked.settled_on_chain) != capture
            || marked.close_requested_at != 0
        {
            self.repo
                .clear_closing(id, &pending.tx_hash)
                .await
                .map_err(store_error)?;
            return Err(MppError::CloseRejected(CloseReason::StateChanged));
        }
        if let Err(error) = broadcast(self.rpc.as_ref(), &tx).await {
            if error == BroadcastError::Reverted {
                self.repo
                    .clear_closing(id, &pending.tx_hash)
                    .await
                    .map_err(store_error)?;
            } else if let Some(receipt) = self.reconcile_close(credential, &marked).await? {
                return Ok(receipt);
            }
            return Err(MppError::ChainTxFailed);
        }
        if !self
            .repo
            .finalize_pending_close(id, &pending.tx_hash, capture)
            .await
            .map_err(store_error)?
        {
            return Err(MppError::CloseRejected(CloseReason::StateChanged));
        }
        let record = self.get(id).await?;
        let mut receipt = receipt(credential, &record, Some(tx.tx_hash));
        receipt.accepted_cumulative = *cumulative_amount;
        Ok(ManagementReceipt { receipt })
    }

    fn check_close_authorization(
        &self,
        id: &[u8; 32],
        descriptor: &ChannelDescriptor,
        cumulative: u128,
        signature: &[u8; 65],
    ) -> Result<(), MppError> {
        let digest =
            close_authorization_digest(&self.config.escrow, self.config.chain_id, id, cumulative)
                .map_err(|_| MppError::CloseRejected(CloseReason::InvalidSignature))?;
        let signer = recover_address(&digest, signature)
            .map_err(|_| MppError::CloseRejected(CloseReason::InvalidSignature))?;
        if signer != descriptor.payer && signer != voucher_signer(descriptor) {
            return Err(MppError::CloseRejected(CloseReason::SignerMismatch));
        }
        Ok(())
    }

    /// A timeout is not proof that a signed transaction failed. Keep the pending fence
    /// until a matching receipt or expiry plus the reconciliation margin establishes it.
    async fn reconcile_close(
        &self,
        credential: &SessionCredential,
        record: &ChannelRecord,
    ) -> Result<Option<ManagementReceipt>, MppError> {
        let pending = record.pending_close.ok_or(MppError::ChannelClosing)?;
        match reconcile_pending_close(self.repo.as_ref(), self.rpc.as_ref(), record, Utc::now())
            .await
            .map_err(|code| {
                if code == "close_state_changed" {
                    MppError::ChannelClosing
                } else {
                    MppError::ChainTxFailed
                }
            })? {
            CloseRecovery::Finalized => {
                let current = self.get(&record.channel_id).await?;
                let mut receipt = receipt(credential, &current, Some(pending.tx_hash));
                receipt.accepted_cumulative = pending.cumulative;
                Ok(Some(ManagementReceipt { receipt }))
            }
            CloseRecovery::Withdrawn => Err(MppError::ChannelClosed),
            CloseRecovery::Pending => Err(MppError::ChainTxFailed),
            CloseRecovery::Reverted | CloseRecovery::Expired => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::mpp::ports::ChannelLifecycle;
    use crate::mpp::session_service::tests::{close, Fixture};
    use crate::mpp::testing::SendMode;

    #[tokio::test]
    async fn invalid_close_signatures_fail_before_missing_channel_lookup() {
        use crate::mpp::error::{CloseReason, MppError, VoucherReason};
        for field in ["signature", "closeSignature"] {
            let f = Fixture::new();
            let mut payload = close(1_250_000);
            payload[field] = serde_json::json!(crate::mpp::session_wire::hex_prefixed(&[0; 65]));
            let credential = f.credential(payload).await;
            let error = f.service.close(&credential).await.err();
            assert_eq!(
                error,
                Some(if field == "signature" {
                    MppError::VoucherRejected(VoucherReason::InvalidSignature)
                } else {
                    MppError::CloseRejected(CloseReason::InvalidSignature)
                })
            );
            assert_eq!(f.rpc.eth_calls(), 0);
            assert_eq!(f.rpc.send_calls(), 0);
        }
    }

    #[tokio::test]
    async fn lost_close_reply_keeps_the_channel_fenced() {
        let f = Fixture::new();
        f.seed().await;
        f.rpc.set_send_mode(SendMode::Unavailable);
        let credential = f.credential(close(1_250_000)).await;
        assert!(f.service.close(&credential).await.is_err());
        let record = f.record().await;
        assert_eq!(record.state, ChannelLifecycle::Closing);
        assert!(record.pending_close.is_some());
    }
}
