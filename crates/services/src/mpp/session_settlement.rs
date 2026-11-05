//! Periodic on-chain settlement of session vouchers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;

use crate::mpp::abi::{decode_channel_state, get_channel_state_calldata, settle_calldata};
use crate::mpp::ports::{
    ChannelLifecycle, ChannelRecord, ChannelRepository, SettlementLease, TempoRpc, TempoSigner,
};
use crate::mpp::session_chain::{broadcast, build_payee_tx, PayeeChain};

/// Scheduling thresholds and bounded ledger page size.
#[derive(Clone)]
pub struct SettlementConfig {
    /// Maximum time between settlements when a settleable voucher exists.
    pub interval: Duration,
    /// Spend added since the last settlement that makes another settlement due.
    pub amount: u128,
    /// Duration of the exclusive settlement lease; defaults to 60 seconds.
    pub lease: Duration,
    /// Maximum records per ledger page; defaults to 100.
    pub page: usize,
}

impl Default for SettlementConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(3600),
            amount: 5_000_000,
            lease: Duration::from_secs(60),
            page: 100,
        }
    }
}

/// Counts from one pass. No channel identifiers or amounts are exposed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SettlementSummary {
    /// Open channels examined.
    pub considered: u64,
    /// Channels meeting either schedule threshold.
    pub due: u64,
    /// Successful broadcasts recorded in the ledger.
    pub settled: u64,
    /// Due channels without a voucher bounded by delivered spend.
    pub no_settleable_voucher: u64,
    /// Due channels whose settlement lease was unavailable.
    pub lease_busy: u64,
    /// Failed operations, including ledger cleanup failures.
    pub failed: u64,
}

fn is_due(record: &ChannelRecord, cfg: &SettlementConfig, now: DateTime<Utc>) -> bool {
    record.spent.saturating_sub(record.last_settlement_spent) >= cfg.amount
        || record.last_settlement_at.is_none_or(|at| {
            now.signed_duration_since(at)
                .to_std()
                .is_ok_and(|age| age >= cfg.interval)
        })
}

async fn release(
    repo: &dyn ChannelRepository,
    id: &[u8; 32],
    lease: SettlementLease,
    summary: &mut SettlementSummary,
) {
    if repo.release_settlement_lease(id, lease).await.is_err() {
        summary.failed += 1;
        tracing::warn!(
            outcome = "settlement_release_failed",
            "MPP settlement lease release failed"
        );
    }
}

enum SettlementAttempt {
    Settled,
    NoVoucher,
    Deferred,
}

/// Settle due open channels in channel-id order. The lease result is authoritative:
/// lifecycle, schedule and voucher are checked again after acquisition. A failed
/// attempt releases its lease. Every attempt refreshes chain state under the lease,
/// so a landed settlement whose ledger write failed is not blindly resubmitted.
pub async fn run_settlement_pass(
    repo: &dyn ChannelRepository,
    rpc: &dyn TempoRpc,
    signer: &dyn TempoSigner,
    chain: PayeeChain,
    cfg: &SettlementConfig,
    now: DateTime<Utc>,
) -> SettlementSummary {
    let started = Instant::now();
    let mut summary = SettlementSummary::default();
    let Ok(lease) = chrono::Duration::from_std(cfg.lease) else {
        summary.failed = 1;
        return summary;
    };
    if cfg.page == 0 || cfg.lease.is_zero() {
        summary.failed = 1;
        return summary;
    }
    let mut after = None;
    loop {
        let records = match repo.list_active(after, cfg.page).await {
            Ok(records) => records,
            Err(_) => {
                summary.failed += 1;
                break;
            }
        };
        let Some(last) = records.last() else {
            break;
        };
        let next = last.channel_id;
        if after.is_some_and(|previous| next <= previous) {
            summary.failed += 1;
            break;
        }
        after = Some(next);
        for record in records {
            if record.state != ChannelLifecycle::Open {
                continue;
            }
            summary.considered += 1;
            if record.chain_id != chain.chain_id
                || record.escrow != chain.escrow
                || record.descriptor.payee != signer.address()
            {
                summary.failed += 1;
                continue;
            }
            // A long page scan must not reuse the pass start as a transaction expiry
            // or claim a lease whose deadline has already passed.
            let Some(now) = i64::try_from(started.elapsed().as_secs())
                .ok()
                .and_then(|seconds| now.checked_add_signed(chrono::Duration::seconds(seconds)))
            else {
                summary.failed += 1;
                continue;
            };
            let (Some(until), Ok(now_unix)) = (
                now.checked_add_signed(lease),
                u64::try_from(now.timestamp()),
            ) else {
                summary.failed += 1;
                continue;
            };
            if !is_due(&record, cfg, now) {
                continue;
            }
            summary.due += 1;
            if record.settleable_voucher().is_none() {
                summary.no_settleable_voucher += 1;
                continue;
            }
            let id = record.channel_id;
            let (current, lease) = match repo.claim_settlement_lease(&id, now, until).await {
                Ok(Some(current)) => current,
                Ok(None) => {
                    summary.lease_busy += 1;
                    continue;
                }
                Err(_) => {
                    summary.failed += 1;
                    continue;
                }
            };
            if current.state != ChannelLifecycle::Open || !is_due(&current, cfg, now) {
                release(repo, &id, lease, &mut summary).await;
                continue;
            }
            if current.settleable_voucher().is_none() {
                summary.no_settleable_voucher += 1;
                release(repo, &id, lease, &mut summary).await;
                continue;
            }
            let attempt = async {
                let bytes = rpc
                    .call(chain.escrow, &get_channel_state_calldata(&id))
                    .await
                    .map_err(|_| "settlement_chain_read_failed")?;
                let state =
                    decode_channel_state(&bytes).map_err(|_| "settlement_chain_decode_failed")?;
                let current = repo
                    .record_chain_state(&id, state, now)
                    .await
                    .map_err(|_| "settlement_chain_record_failed")?
                    .ok_or("settlement_channel_missing")?;
                if state.deposit == 0
                    || current.state != ChannelLifecycle::Open
                    || !is_due(&current, cfg, now)
                {
                    return Ok(SettlementAttempt::Deferred);
                }
                let Some(voucher) = current.settleable_voucher() else {
                    return Ok(SettlementAttempt::NoVoucher);
                };
                let calldata =
                    settle_calldata(&current.descriptor, voucher.cumulative, &voucher.signature)
                        .map_err(|_| "settlement_calldata_failed")?;
                let tx = build_payee_tx(chain, signer, rpc, calldata, now_unix)
                    .await
                    .map_err(|error| error.code())?;
                // Recheck independently observed finalization before broadcast; a live
                // settlement lease atomically excludes the close transition.
                let latest = repo
                    .get(&id)
                    .await
                    .map_err(|_| "settlement_reread_failed")?
                    .ok_or("settlement_channel_missing")?;
                if latest.state != ChannelLifecycle::Open
                    || latest.settled_on_chain >= voucher.cumulative
                {
                    return Ok(SettlementAttempt::Deferred);
                }
                broadcast(rpc, &tx).await.map_err(|error| error.code())?;
                if !repo
                    .record_settlement(&id, lease, voucher.cumulative, now)
                    .await
                    .map_err(|_| "settlement_record_failed")?
                {
                    return Ok(SettlementAttempt::Deferred);
                }
                Ok(SettlementAttempt::Settled)
            };
            // Bound broadcast work below the lease lifetime; persistence and release
            // also compare the lease fence after any reassignment.
            let result = tokio::time::timeout(cfg.lease / 2, attempt)
                .await
                .unwrap_or(Err("settlement_timeout"));
            match result {
                Ok(SettlementAttempt::Settled) => summary.settled += 1,
                Ok(outcome) => {
                    if matches!(outcome, SettlementAttempt::NoVoucher) {
                        summary.no_settleable_voucher += 1;
                    }
                    release(repo, &id, lease, &mut summary).await;
                }
                Err(outcome) => {
                    summary.failed += 1;
                    tracing::warn!(outcome, "MPP settlement failed");
                    release(repo, &id, lease, &mut summary).await;
                }
            }
        }
    }
    summary
}

/// Spawn settlement on the current Tokio runtime. Runs immediately, then every quarter
/// of the configured settlement interval, with a minimum cadence of 60 seconds.
pub fn spawn_settlement_task(
    repo: Arc<dyn ChannelRepository>,
    rpc: Arc<dyn TempoRpc>,
    signer: Arc<dyn TempoSigner>,
    chain: PayeeChain,
    cfg: SettlementConfig,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval((cfg.interval / 4).max(Duration::from_secs(60)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let summary = run_settlement_pass(
                repo.as_ref(),
                rpc.as_ref(),
                signer.as_ref(),
                chain,
                &cfg,
                Utc::now(),
            )
            .await;
            tracing::info!(
                considered = summary.considered,
                due = summary.due,
                settled = summary.settled,
                no_settleable_voucher = summary.no_settleable_voucher,
                lease_busy = summary.lease_busy,
                failed = summary.failed,
                "MPP settlement pass completed"
            );
        }
    })
}
