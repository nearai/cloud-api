//! Cooperative close and reconciliation of uncertain broadcasts.

use chrono::Utc;

use crate::mpp::abi::{self, ChannelDescriptor};
use crate::mpp::channel::voucher_signer;
use crate::mpp::eip712::close_authorization_digest;
use crate::mpp::error::{CloseReason, MppError, SessionMismatch};
use crate::mpp::ports::{ChannelLifecycle, ChannelRecord, CloseTransition, PendingClose};
use crate::mpp::secp256k1::recover_address;
use crate::mpp::session_chain::{broadcast, build_payee_tx, BroadcastError, PayeeChain};
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
        let _gate = self.gate(id).await?;
        let mut record = self.get(id).await?;
        self.bind(&record, descriptor)?;
        self.check_close_authorization(id, descriptor, *cumulative_amount, close_signature)?;
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
        self.check_voucher(
            &record,
            *cumulative_amount,
            signature,
            credential.min_voucher_delta,
            true,
        )?;
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
            .mark_closing(id, pending)
            .await
            .map_err(store_error)?
        {
            CloseTransition::Marked(record) => record,
            CloseTransition::NotOpen => return Err(MppError::ChannelClosing),
            CloseTransition::Missing => return Err(MppError::ChannelNotFound),
        };
        // The atomic transition fences further reservations. Recheck all capture-sensitive
        // state returned by that transition; another instance may have charged during build.
        if marked.reserved != 0
            || marked.spent.max(marked.settled_on_chain) != capture
            || marked.close_requested_at != 0
        {
            self.repo.clear_closing(id).await.map_err(store_error)?;
            return Err(MppError::CloseRejected(CloseReason::StateChanged));
        }
        if let Err(error) = broadcast(self.rpc.as_ref(), &tx).await {
            if error == BroadcastError::Reverted {
                self.repo.clear_closing(id).await.map_err(store_error)?;
            } else if let Some(receipt) = self.reconcile_close(credential, &marked).await? {
                return Ok(receipt);
            }
            return Err(MppError::ChainTxFailed);
        }
        self.repo.finalize(id, capture).await.map_err(store_error)?;
        let record = self.get(id).await?;
        let mut receipt = receipt(credential, &record, Some(tx.tx_hash));
        receipt.accepted_cumulative = receipt.accepted_cumulative.max(*cumulative_amount);
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
        let mined = self
            .rpc
            .get_transaction_receipt(pending.tx_hash)
            .await
            .map_err(|_| MppError::ChainTxFailed)?;
        if let Some(mined) = mined {
            if mined.tx_hash != pending.tx_hash {
                return Err(MppError::ChainTxFailed);
            }
            if mined.success {
                self.repo
                    .finalize(&record.channel_id, pending.capture)
                    .await
                    .map_err(store_error)?;
                let current = self.get(&record.channel_id).await?;
                let mut receipt = receipt(credential, &current, Some(pending.tx_hash));
                receipt.accepted_cumulative = receipt.accepted_cumulative.max(pending.cumulative);
                return Ok(Some(ManagementReceipt { receipt }));
            }
            self.repo
                .clear_closing(&record.channel_id)
                .await
                .map_err(store_error)?;
            return Ok(None);
        }
        let now = u64::try_from(Utc::now().timestamp()).map_err(|_| MppError::Config)?;
        let deadline = pending
            .valid_before
            .checked_add(30)
            .ok_or(MppError::Config)?;
        if now <= deadline {
            return Err(MppError::ChainTxFailed);
        }
        // A read after expiry detects a successful close whose receipt was unavailable,
        // or a payer withdrawal. Never revive a zero-deposit channel.
        let state = self.read_chain(&record.channel_id).await?;
        if state.deposit == 0 {
            self.repo
                .finalize(&record.channel_id, state.settled)
                .await
                .map_err(store_error)?;
            return Err(MppError::ChannelClosed);
        }
        self.repo
            .record_chain_state(&record.channel_id, state, Utc::now())
            .await
            .map_err(store_error)?;
        self.repo
            .clear_closing(&record.channel_id)
            .await
            .map_err(store_error)?;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use crate::mpp::ports::ChannelLifecycle;
    use crate::mpp::session_service::tests::{close, Fixture};
    use crate::mpp::testing::SendMode;

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
