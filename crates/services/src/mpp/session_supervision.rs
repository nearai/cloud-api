//! Close supervision: payee closes that capture delivered spend when a payer requests a
//! close on chain, and finalization of withdrawn channels.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures::{stream, StreamExt};
use tokio::task::JoinHandle;

use crate::mpp::abi::{
    close_calldata, decode_channel_states_batch, get_channel_states_batch_calldata, ChannelState,
    MAX_BATCH_CHANNELS,
};
use crate::mpp::ports::{
    chain_read_claim_at, chain_read_is_fresh, ChannelLifecycle, ChannelRecord, ChannelRepository,
    CloseTransition, PendingClose, RetainedVoucher, TempoRpc, TempoSigner,
};
use crate::mpp::session_chain::{broadcast, build_payee_tx, PayeeChain};
use crate::mpp::session_close_reconciliation::{reconcile_pending_close, CloseRecovery};

const MAX_IN_FLIGHT: usize = 8;
pub const PAYEE_CLOSE_DEADLINE_SECS: u64 = 600;

/// Cadence, batching and stale-reservation policy for close supervision.
#[derive(Clone)]
pub struct SupervisionConfig {
    /// Time between passes; defaults to 60 seconds.
    pub every: Duration,
    /// Maximum records per ledger page; defaults to 100.
    pub page: usize,
    /// Warn when a pass takes longer than this; defaults to 120 seconds.
    pub warn_after: Duration,
    /// Release reservations older than this; defaults to two hours.
    pub stale_reservation_after: Duration,
}

impl Default for SupervisionConfig {
    fn default() -> Self {
        Self {
            every: Duration::from_secs(60),
            page: 100,
            warn_after: Duration::from_secs(120),
            stale_reservation_after: Duration::from_secs(2 * 60 * 60),
        }
    }
}

/// Counts from one supervision pass, safe to log without channel data.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SupervisionSummary {
    /// Active ledger records scanned, including records whose chain read failed.
    pub scanned: u64,
    /// Batch `eth_call` attempts, including failed calls.
    pub chain_reads: u64,
    /// Zero-deposit channels successfully finalized in the ledger.
    pub finalized_withdrawn: u64,
    /// Open channels with an on-chain close request.
    pub close_requested: u64,
    /// Close transactions handed to broadcast after persisting pending state.
    pub closes_submitted: u64,
    /// Close attempts that failed during build, transition, broadcast or finalization.
    pub closes_failed: u64,
    /// Close requests deferred while live reservations can still drain.
    pub reservation_deferrals: u64,
    /// Requested closes with no delivered spend above the settled amount.
    pub nothing_to_capture: u64,
    /// Expired reservations released during this pass.
    pub stale_reservations_released: u64,
    /// Pending closes finalized from a matching successful receipt.
    pub reconciled_finalized: u64,
    /// Pending closes cleared after a matching reverted receipt.
    pub reconciled_reverted: u64,
    /// Pending closes cleared after expiry plus the nonce safety margin.
    pub reconciled_expired: u64,
    /// Pending closes with no receipt, still within their validity window.
    pub reconciliation_pending: u64,
    /// Receipt reads, receipt validation or reconciliation writes that failed.
    pub reconciliation_failed: u64,
}

async fn reconcile_close(
    repo: &dyn ChannelRepository,
    rpc: &dyn TempoRpc,
    record: &ChannelRecord,
    now: DateTime<Utc>,
    summary: &mut SupervisionSummary,
) {
    match reconcile_pending_close(repo, rpc, record, now).await {
        Ok(CloseRecovery::Finalized | CloseRecovery::Withdrawn) => {
            summary.reconciled_finalized += 1
        }
        Ok(CloseRecovery::Reverted) => summary.reconciled_reverted += 1,
        Ok(CloseRecovery::Expired) => summary.reconciled_expired += 1,
        Ok(CloseRecovery::Pending) => summary.reconciliation_pending += 1,
        Err(outcome) => {
            summary.reconciliation_failed += 1;
            tracing::warn!(outcome, "MPP close reconciliation failed");
        }
    }
}

fn pass_time(now: DateTime<Utc>, started: Instant) -> Option<DateTime<Utc>> {
    i64::try_from(started.elapsed().as_secs())
        .ok()
        .and_then(|seconds| now.checked_add_signed(chrono::Duration::seconds(seconds)))
}

fn capture_at(record: &ChannelRecord, now: DateTime<Utc>) -> Option<u128> {
    if record.reserved == 0 {
        return Some(record.delivered());
    }
    let now = u64::try_from(now.timestamp()).ok()?;
    let deadline = record
        .close_requested_at
        .checked_add(PAYEE_CLOSE_DEADLINE_SECS)?;
    (now >= deadline).then_some(record.spent)
}

impl SupervisionSummary {
    fn add(&mut self, other: Self) {
        self.finalized_withdrawn += other.finalized_withdrawn;
        self.close_requested += other.close_requested;
        self.closes_submitted += other.closes_submitted;
        self.closes_failed += other.closes_failed;
        self.nothing_to_capture += other.nothing_to_capture;
        self.reservation_deferrals += other.reservation_deferrals;
        self.reconciled_finalized += other.reconciled_finalized;
        self.reconciled_reverted += other.reconciled_reverted;
        self.reconciled_expired += other.reconciled_expired;
        self.reconciliation_pending += other.reconciliation_pending;
        self.reconciliation_failed += other.reconciliation_failed;
    }
}

async fn supervise_channel(
    repo: &dyn ChannelRepository,
    rpc: &dyn TempoRpc,
    signer: &dyn TempoSigner,
    chain: PayeeChain,
    observation: (&ChannelRecord, ChannelState),
    now: DateTime<Utc>,
    summary: &mut SupervisionSummary,
) {
    let (record, state) = observation;
    let id = record.channel_id;
    // Refreshing the row also fences new vouchers/reservations after a close request.
    let current = match repo.record_chain_state(&id, state, now).await {
        Ok(Some(current)) => current,
        Ok(None) => return,
        Err(_) => {
            tracing::warn!(
                outcome = "chain_state_record_failed",
                "MPP supervision state write failed"
            );
            return;
        }
    };
    if current.state == ChannelLifecycle::Finalized {
        return;
    }
    if current.state == ChannelLifecycle::Closing {
        reconcile_close(repo, rpc, &current, now, summary).await;
        return;
    }
    if state.deposit == 0 {
        match repo.finalize(&id, state.settled).await {
            Ok(()) => summary.finalized_withdrawn += 1,
            Err(_) => tracing::warn!(
                outcome = "withdrawal_finalize_failed",
                "MPP supervision finalization failed"
            ),
        }
        return;
    }
    close_requested_channel(repo, rpc, signer, chain, current, now, summary).await;
}

async fn close_requested_channel(
    repo: &dyn ChannelRepository,
    rpc: &dyn TempoRpc,
    signer: &dyn TempoSigner,
    chain: PayeeChain,
    current: ChannelRecord,
    now: DateTime<Utc>,
    summary: &mut SupervisionSummary,
) {
    if current.close_requested_at == 0 || current.state != ChannelLifecycle::Open {
        return;
    }
    let id = current.channel_id;
    summary.close_requested += 1;
    let Some(capture) = capture_at(&current, now) else {
        summary.reservation_deferrals += 1;
        return;
    };
    if capture <= current.settled_on_chain {
        summary.nothing_to_capture += 1;
        return;
    }
    let highest = RetainedVoucher {
        cumulative: current.highest_cumulative,
        signature: current.highest_signature,
    };
    let voucher = std::iter::once(highest)
        .chain(current.retained_vouchers.iter().copied())
        .filter(|voucher| voucher.cumulative >= capture)
        .min_by_key(|voucher| voucher.cumulative);
    let build = async {
        let voucher = voucher.ok_or("close_voucher_missing")?;
        let calldata = close_calldata(
            &current.descriptor,
            voucher.cumulative,
            capture,
            &voucher.signature,
        )
        .map_err(|_| "close_calldata_failed")?;
        let now_unix = u64::try_from(now.timestamp()).map_err(|_| "close_time_invalid")?;
        let tx = build_payee_tx(chain, signer, rpc, calldata, now_unix)
            .await
            .map_err(|error| error.code())?;
        let pending = PendingClose {
            tx_hash: tx.tx_hash,
            cumulative: voucher.cumulative,
            capture,
            valid_before: tx.valid_before,
        };
        Ok::<_, &'static str>((tx, pending))
    }
    .await;
    let (tx, pending) = match build {
        Ok(built) => built,
        Err(outcome) => {
            summary.closes_failed += 1;
            tracing::warn!(outcome, "MPP supervised close build failed");
            return;
        }
    };
    match repo.mark_closing(&id, pending, now).await {
        Ok(CloseTransition::Marked(marked)) => {
            // An in-flight inference can finish while gas price/signing is awaited.
            // Never close at the old capture if delivered spend changed meanwhile.
            if marked.close_requested_at == 0
                || capture_at(&marked, now) != Some(capture)
                || marked.settled_on_chain >= capture
            {
                summary.closes_failed += 1;
                tracing::warn!(
                    outcome = "close_capture_changed",
                    "MPP supervised close deferred"
                );
                if repo.clear_closing(&id, &pending.tx_hash).await.is_err() {
                    tracing::warn!(
                        outcome = "clear_closing_failed",
                        "MPP supervised close cleanup failed"
                    );
                }
                return;
            }
        }
        Ok(
            CloseTransition::NotOpen
            | CloseTransition::Missing
            | CloseTransition::SettlementInFlight,
        ) => return,
        Err(_) => {
            summary.closes_failed += 1;
            tracing::warn!(
                outcome = "mark_closing_failed",
                "MPP supervised close transition failed"
            );
            return;
        }
    }
    summary.closes_submitted += 1;
    match broadcast(rpc, &tx).await {
        Ok(_) => {
            if repo
                .finalize_pending_close(&id, &pending.tx_hash, capture)
                .await
                .is_err()
            {
                // Keep the pending hash: the transaction landed but persistence failed.
                summary.closes_failed += 1;
                tracing::warn!(
                    outcome = "close_finalize_failed",
                    "MPP supervised close finalization failed"
                );
            }
        }
        Err(error) => {
            summary.closes_failed += 1;
            tracing::warn!(
                outcome = error.code(),
                "MPP supervised close broadcast failed"
            );
            if error.definitively_failed()
                && repo.clear_closing(&id, &pending.tx_hash).await.is_err()
            {
                tracing::warn!(
                    outcome = "clear_closing_failed",
                    "MPP supervised close cleanup failed"
                );
            }
        }
    }
}

/// Read active channels in bounded batches, finalize withdrawals and answer payer close
/// requests at delivered spend. A failed batch is skipped; later batches still run.
/// Stale reservations are released once per pass, even when a page read fails.
pub async fn run_supervision_pass(
    repo: &dyn ChannelRepository,
    rpc: &dyn TempoRpc,
    signer: &dyn TempoSigner,
    chain: PayeeChain,
    cfg: &SupervisionConfig,
    now: DateTime<Utc>,
) -> SupervisionSummary {
    let started = Instant::now();
    let mut summary = SupervisionSummary::default();
    let Ok(chain_freshness) = chrono::Duration::from_std(cfg.every.max(Duration::from_secs(1)))
    else {
        tracing::warn!(
            outcome = "supervision_interval_invalid",
            "MPP supervision interval invalid"
        );
        return summary;
    };
    let cutoff = chrono::Duration::from_std(cfg.stale_reservation_after)
        .ok()
        .and_then(|age| now.checked_sub_signed(age));
    if let Some(cutoff) = cutoff {
        match repo.release_stale_reservations(cutoff).await {
            Ok(count) => summary.stale_reservations_released = count,
            Err(_) => tracing::warn!(
                outcome = "stale_reservations_release_failed",
                "MPP stale reservation release failed"
            ),
        }
    } else {
        tracing::warn!(
            outcome = "stale_reservations_cutoff_invalid",
            "MPP stale reservation cutoff invalid"
        );
    }
    let mut after = None;
    loop {
        if cfg.page == 0 {
            tracing::warn!(
                outcome = "supervision_page_invalid",
                "MPP supervision page invalid"
            );
            break;
        }
        let records = match repo.list_active(after, cfg.page).await {
            Ok(records) => records,
            Err(_) => {
                tracing::warn!(
                    outcome = "supervision_list_failed",
                    "MPP supervision page failed"
                );
                break;
            }
        };
        let Some(last) = records.last() else {
            break;
        };
        let next = last.channel_id;
        if after.is_some_and(|previous| next <= previous) {
            tracing::warn!(
                outcome = "supervision_cursor_invalid",
                "MPP supervision cursor failed"
            );
            break;
        }
        after = Some(next);
        for _ in &records {
            summary.scanned += 1;
        }
        let records: Vec<_> = records
            .into_iter()
            .filter(|record| {
                let matches = record.chain_id == chain.chain_id
                    && record.escrow == chain.escrow
                    && record.descriptor.payee == signer.address();
                if !matches {
                    tracing::warn!(
                        outcome = "supervision_chain_mismatch",
                        "MPP supervision channel skipped"
                    );
                }
                matches
            })
            .collect();
        let (closing, open): (Vec<_>, Vec<_>) = records
            .into_iter()
            .partition(|record| record.state == ChannelLifecycle::Closing);
        let Some(observed_at) = pass_time(now, started) else {
            break;
        };
        let (known_closes, open): (Vec<_>, Vec<_>) = open.into_iter().partition(|record| {
            record.state == ChannelLifecycle::Open
                && record.close_requested_at != 0
                && chain_read_is_fresh(record.last_chain_read_at, observed_at, chain_freshness)
        });
        // A known close request keeps its deadline even when a recent HTTP refresh
        // makes another state read unnecessary. Run these before receipt recovery.
        let mut known_closes = stream::iter(known_closes)
            .map(|record| async move {
                let mut summary = SupervisionSummary::default();
                if let Some(at) = pass_time(now, started) {
                    close_requested_channel(repo, rpc, signer, chain, record, at, &mut summary)
                        .await;
                }
                summary
            })
            .buffer_unordered(MAX_IN_FLIGHT);
        while let Some(result) = known_closes.next().await {
            summary.add(result);
        }
        // Acquire read claims after recovery so a slow receipt cannot age a claim
        // before its associated state read starts.
        let mut recoveries = stream::iter(closing)
            .map(|record| async move {
                let mut summary = SupervisionSummary::default();
                if let Some(at) = pass_time(now, started) {
                    reconcile_close(repo, rpc, &record, at, &mut summary).await;
                } else {
                    summary.reconciliation_failed += 1;
                }
                summary
            })
            .buffer_unordered(MAX_IN_FLIGHT);
        while let Some(result) = recoveries.next().await {
            summary.add(result);
        }
        let mut claimed = Vec::with_capacity(open.len());
        let Some(claimed_at) = pass_time(now, started) else {
            break;
        };
        let Some(claim_deadline) = chain_read_claim_at(claimed_at, chain_freshness) else {
            break;
        };
        for record in open {
            // A fresh observation needs neither another row write nor an RPC.
            if chain_read_is_fresh(record.last_chain_read_at, claimed_at, chain_freshness) {
                continue;
            }
            match repo
                .claim_chain_read(&record.channel_id, claimed_at, chain_freshness)
                .await
            {
                Ok(true) => claimed.push(record),
                Ok(false) => {}
                Err(_) => tracing::warn!(
                    outcome = "chain_read_claim_failed",
                    "MPP supervision claim failed"
                ),
            }
        }
        for batch in claimed.chunks(MAX_BATCH_CHANNELS) {
            let ids: Vec<_> = batch.iter().map(|record| record.channel_id).collect();
            let states = if let Ok(data) = get_channel_states_batch_calldata(&ids) {
                summary.chain_reads += 1;
                match rpc.call(chain.escrow, &data).await {
                    Ok(bytes) => decode_channel_states_batch(&bytes).ok(),
                    Err(_) => None,
                }
            } else {
                None
            };
            let Some(states) = states.filter(|states| states.len() == batch.len()) else {
                tracing::warn!(
                    outcome = "supervision_chain_read_failed",
                    "MPP supervision batch skipped"
                );
                for record in batch {
                    if repo
                        .clear_chain_read_claim(&record.channel_id, claim_deadline)
                        .await
                        .is_err()
                    {
                        tracing::warn!(
                            outcome = "chain_read_claim_clear_failed",
                            "MPP supervision claim cleanup failed"
                        );
                    }
                }
                continue;
            };
            let observations: Vec<_> = batch.iter().cloned().zip(states).collect();
            let mut operations = stream::iter(observations)
                .map(|(record, state)| async move {
                    let mut summary = SupervisionSummary::default();
                    if let Some(at) = pass_time(now, started) {
                        supervise_channel(
                            repo,
                            rpc,
                            signer,
                            chain,
                            (&record, state),
                            at,
                            &mut summary,
                        )
                        .await;
                    }
                    summary
                })
                .buffer_unordered(MAX_IN_FLIGHT);
            while let Some(result) = operations.next().await {
                summary.add(result);
            }
        }
    }
    if started.elapsed() > cfg.warn_after {
        tracing::warn!(
            outcome = "supervision_slow",
            scanned = summary.scanned,
            chain_reads = summary.chain_reads,
            closes_submitted = summary.closes_submitted,
            closes_failed = summary.closes_failed,
            "MPP supervision pass exceeded warning threshold"
        );
    }
    summary
}

/// Spawn supervision on the current Tokio runtime. Runs immediately and then every
/// configured interval. A zero interval is clamped to one second to avoid a busy loop.
pub fn spawn_supervision_task(
    repo: Arc<dyn ChannelRepository>,
    rpc: Arc<dyn TempoRpc>,
    signer: Arc<dyn TempoSigner>,
    chain: PayeeChain,
    cfg: SupervisionConfig,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let every = if cfg.every.is_zero() {
            Duration::from_secs(1)
        } else {
            cfg.every
        };
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let summary = run_supervision_pass(
                repo.as_ref(),
                rpc.as_ref(),
                signer.as_ref(),
                chain,
                &cfg,
                Utc::now(),
            )
            .await;
            tracing::info!(
                scanned = summary.scanned,
                chain_reads = summary.chain_reads,
                finalized_withdrawn = summary.finalized_withdrawn,
                close_requested = summary.close_requested,
                closes_submitted = summary.closes_submitted,
                closes_failed = summary.closes_failed,
                reservation_deferrals = summary.reservation_deferrals,
                nothing_to_capture = summary.nothing_to_capture,
                stale_reservations_released = summary.stale_reservations_released,
                reconciled_finalized = summary.reconciled_finalized,
                reconciled_reverted = summary.reconciled_reverted,
                reconciled_expired = summary.reconciled_expired,
                reconciliation_pending = summary.reconciliation_pending,
                reconciliation_failed = summary.reconciliation_failed,
                "MPP supervision pass completed"
            );
        }
    })
}
