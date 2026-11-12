//! Shared fail-closed recovery for cooperative and supervised close transactions.

use chrono::{DateTime, Utc};

use super::abi::{decode_channel_state, get_channel_state_calldata};
use super::ports::{ChannelRecord, ChannelRepository, TempoRpc};

pub(crate) enum CloseRecovery {
    Finalized,
    Withdrawn,
    Reverted,
    Expired,
    Pending,
}

pub(crate) async fn reconcile_pending_close(
    repo: &dyn ChannelRepository,
    rpc: &dyn TempoRpc,
    record: &ChannelRecord,
    now: DateTime<Utc>,
) -> Result<CloseRecovery, &'static str> {
    let pending = record.pending_close.ok_or("close_pending_missing")?;
    let receipt = rpc
        .get_transaction_receipt(pending.tx_hash)
        .await
        .map_err(|_| "close_receipt_failed")?;
    if let Some(receipt) = receipt {
        if receipt.tx_hash != pending.tx_hash {
            return Err("close_receipt_mismatch");
        }
        let changed = if receipt.success {
            repo.finalize_pending_close(&record.channel_id, &pending.tx_hash, pending.capture)
                .await
                .map_err(|_| "close_finalize_failed")?
        } else {
            repo.clear_closing(&record.channel_id, &pending.tx_hash)
                .await
                .map_err(|_| "clear_closing_failed")?
        };
        if !changed {
            return Err("close_state_changed");
        }
        return Ok(if receipt.success {
            CloseRecovery::Finalized
        } else {
            CloseRecovery::Reverted
        });
    }
    let now_unix = u64::try_from(now.timestamp()).map_err(|_| "close_time_invalid")?;
    let deadline = pending
        .valid_before
        .checked_add(30)
        .ok_or("close_time_invalid")?;
    if now_unix <= deadline {
        return Ok(CloseRecovery::Pending);
    }
    // Missing receipts never prove failure. After expiry, read the authoritative
    // channel before lifting the fence; a successful close may have lost its receipt.
    let bytes = rpc
        .call(
            record.escrow,
            &get_channel_state_calldata(&record.channel_id),
        )
        .await
        .map_err(|_| "close_chain_read_failed")?;
    let state = decode_channel_state(&bytes).map_err(|_| "close_chain_decode_failed")?;
    if state.deposit == 0 {
        repo.finalize(&record.channel_id, state.settled)
            .await
            .map_err(|_| "close_finalize_failed")?;
        return Ok(CloseRecovery::Withdrawn);
    }
    repo.record_chain_state(&record.channel_id, state, now)
        .await
        .map_err(|_| "close_chain_record_failed")?
        .ok_or("close_channel_missing")?;
    if !repo
        .clear_closing(&record.channel_id, &pending.tx_hash)
        .await
        .map_err(|_| "clear_closing_failed")?
    {
        return Err("close_state_changed");
    }
    Ok(CloseRecovery::Expired)
}
