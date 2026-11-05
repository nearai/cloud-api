//! Close supervision: payee closes that capture delivered spend when a payer requests a
//! close on chain, and finalization of withdrawn channels.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;

use crate::mpp::abi::{
    close_calldata, decode_channel_states_batch, get_channel_states_batch_calldata, ChannelState,
    MAX_BATCH_CHANNELS,
};
use crate::mpp::ports::{
    ChannelLifecycle, ChannelRecord, ChannelRepository, CloseTransition, PendingClose,
    RetainedVoucher, TempoRpc, TempoSigner,
};
use crate::mpp::session_chain::{broadcast, build_payee_tx, PayeeChain};

/// Cadence, batching and stale-reservation policy for close supervision.
#[derive(Clone)]
pub struct SupervisionConfig {
    /// Time between passes; defaults to 60 seconds.
    pub every: Duration,
    /// Maximum records per ledger page; defaults to 100.
    pub page: usize,
    /// Warn when a pass takes longer than this; defaults to 120 seconds.
    pub warn_after: Duration,
    /// Release reservations older than this; defaults to 15 minutes.
    pub stale_reservation_after: Duration,
}

impl Default for SupervisionConfig {
    fn default() -> Self {
        Self {
            every: Duration::from_secs(60),
            page: 100,
            warn_after: Duration::from_secs(120),
            stale_reservation_after: Duration::from_secs(15 * 60),
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
    /// Requested closes with no delivered spend above the settled amount.
    pub nothing_to_capture: u64,
    /// Expired reservations released during this pass.
    pub stale_reservations_released: u64,
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
    if state.close_requested_at == 0 || current.state != ChannelLifecycle::Open {
        return;
    }
    summary.close_requested += 1;
    let capture = current.delivered();
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
    match repo.mark_closing(&id, pending).await {
        Ok(CloseTransition::Marked(marked)) => {
            // An in-flight inference can finish while gas price/signing is awaited.
            // Never close at the old capture if delivered spend changed meanwhile.
            if marked.delivered() != capture || marked.settled_on_chain >= capture {
                summary.closes_failed += 1;
                tracing::warn!(
                    outcome = "close_capture_changed",
                    "MPP supervised close deferred"
                );
                if repo.clear_closing(&id).await.is_err() {
                    tracing::warn!(
                        outcome = "clear_closing_failed",
                        "MPP supervised close cleanup failed"
                    );
                }
                return;
            }
        }
        Ok(CloseTransition::NotOpen | CloseTransition::Missing) => return,
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
            if repo.finalize(&id, capture).await.is_err() {
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
            if repo.clear_closing(&id).await.is_err() {
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
        for batch in records.chunks(MAX_BATCH_CHANNELS) {
            let ids: Vec<_> = batch.iter().map(|record| record.channel_id).collect();
            let Ok(data) = get_channel_states_batch_calldata(&ids) else {
                continue;
            };
            summary.chain_reads += 1;
            let states = match rpc.call(chain.escrow, &data).await {
                Ok(bytes) => decode_channel_states_batch(&bytes).ok(),
                Err(_) => None,
            };
            let Some(states) = states.filter(|states| states.len() == batch.len()) else {
                tracing::warn!(
                    outcome = "supervision_chain_read_failed",
                    "MPP supervision batch skipped"
                );
                continue;
            };
            for (record, state) in batch.iter().zip(states) {
                let Some(at) = i64::try_from(started.elapsed().as_secs())
                    .ok()
                    .and_then(|seconds| now.checked_add_signed(chrono::Duration::seconds(seconds)))
                else {
                    tracing::warn!(
                        outcome = "supervision_time_invalid",
                        "MPP supervision channel skipped"
                    );
                    continue;
                };
                supervise_channel(repo, rpc, signer, chain, (record, state), at, &mut summary)
                    .await;
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
                nothing_to_capture = summary.nothing_to_capture,
                stale_reservations_released = summary.stale_reservations_released,
                "MPP supervision pass completed"
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::abi::{uint_word, GET_CHANNEL_STATES_BATCH_SELECTOR};
    use crate::mpp::channel_memory::InMemoryChannelRepository;
    use crate::mpp::ports::{NewChannel, ReserveOutcome, RpcError, RpcReceipt};
    use crate::mpp::session_vectors::Vectors;
    use crate::mpp::signer::LocalSigner;
    use crate::mpp::tempo_tx::{decode_signed, tx_hash};
    use crate::mpp::testing::{FakeTempo, SendMode};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn chain() -> PayeeChain {
        let v = Vectors::load();
        PayeeChain {
            chain_id: v.chain_id(),
            escrow: v.escrow(),
            fee_token: v.fixed("token"),
        }
    }

    fn signer() -> LocalSigner {
        LocalSigner::new(Vectors::load().secret_key("payeeKey"))
    }

    async fn open(repo: &InMemoryChannelRepository, id: [u8; 32]) {
        let v = Vectors::load();
        let mut descriptor = v.descriptor();
        descriptor.salt = id;
        repo.open_or_refresh(NewChannel {
            channel_id: id,
            chain_id: v.chain_id(),
            escrow: v.escrow(),
            descriptor,
            deposit: 1000,
            settled_on_chain: 0,
            highest_cumulative: 60,
            highest_signature: v.fixed("voucher.signature"),
        })
        .await
        .unwrap();
        for cumulative in [100, 200] {
            repo.accept_voucher(&id, cumulative, v.fixed("voucher.signature"))
                .await
                .unwrap();
        }
    }

    async fn reserve(repo: &InMemoryChannelRepository, id: &[u8; 32], amount: u128) -> Uuid {
        let request = Uuid::new_v4();
        assert!(matches!(
            repo.reserve(id, request, amount).await.unwrap(),
            ReserveOutcome::Reserved(_)
        ));
        request
    }

    async fn spend(repo: &InMemoryChannelRepository, id: &[u8; 32], amount: u128) {
        let request = reserve(repo, id, amount).await;
        repo.charge(request, amount, 1).await.unwrap();
    }

    fn encode_states(states: &[ChannelState]) -> Vec<u8> {
        let mut bytes = uint_word(32).to_vec();
        bytes.extend(uint_word(states.len() as u128));
        for state in states {
            bytes.extend(uint_word(state.settled));
            bytes.extend(uint_word(state.deposit));
            bytes.extend(uint_word(u128::from(state.close_requested_at)));
        }
        bytes
    }

    fn answer(rpc: &FakeTempo, state: ChannelState) {
        rpc.set_call_handler(Box::new(move |to, data| {
            assert_eq!(to, chain().escrow);
            assert_eq!(data[..4], GET_CHANNEL_STATES_BATCH_SELECTOR);
            let count = (data.len() - 68) / 32;
            assert!(count <= MAX_BATCH_CHANNELS);
            Ok(encode_states(&vec![state; count]))
        }));
    }

    fn requested() -> ChannelState {
        ChannelState {
            settled: 0,
            deposit: 1000,
            close_requested_at: 1_799_999_990,
        }
    }

    // The fake observes the ledger *inside* send, making persist-before-send an
    // ordering assertion rather than an assertion on eventual final state.
    struct OrderedRpc {
        fake: FakeTempo,
        repo: Arc<InMemoryChannelRepository>,
        id: [u8; 32],
        observed_closing: AtomicUsize,
        cumulative: u128,
        capture: u128,
        charge_during_build: Option<(Uuid, u128)>,
    }

    #[async_trait]
    impl TempoRpc for OrderedRpc {
        async fn send_raw_transaction_sync(&self, raw: &[u8]) -> Result<RpcReceipt, RpcError> {
            let record = self.repo.get(&self.id).await.unwrap().unwrap();
            assert_eq!(record.state, ChannelLifecycle::Closing);
            let pending = record.pending_close.unwrap();
            assert_eq!(pending.tx_hash, tx_hash(raw));
            assert_eq!(
                (pending.cumulative, pending.capture),
                (self.cumulative, self.capture)
            );
            let (tx, _) = decode_signed(raw).unwrap();
            assert_eq!(tx.valid_before, Some(pending.valid_before));
            assert_eq!(
                tx.calls[0].input,
                close_calldata(
                    &record.descriptor,
                    self.cumulative,
                    self.capture,
                    &record.highest_signature
                )
                .unwrap()
            );
            self.observed_closing.fetch_add(1, Ordering::SeqCst);
            self.fake.mine(RpcReceipt {
                tx_hash: tx_hash(raw),
                block_number: 1,
                success: true,
                from: signer().address(),
                logs: vec![],
            });
            self.fake.send_raw_transaction_sync(raw).await
        }
        async fn get_transaction_receipt(
            &self,
            hash: [u8; 32],
        ) -> Result<Option<RpcReceipt>, RpcError> {
            self.fake.get_transaction_receipt(hash).await
        }
        async fn get_block_timestamp(&self, number: u64) -> Result<Option<u64>, RpcError> {
            self.fake.get_block_timestamp(number).await
        }
        async fn call(&self, to: [u8; 20], data: &[u8]) -> Result<Vec<u8>, RpcError> {
            self.fake.call(to, data).await
        }
        async fn gas_price(&self) -> Result<u128, RpcError> {
            if let Some((request, amount)) = self.charge_during_build {
                self.repo.charge(request, amount, 1).await.unwrap();
            }
            self.fake.gas_price().await
        }
    }

    #[tokio::test]
    async fn one_hundred_fifty_channels_use_two_batch_calls() {
        let repo = InMemoryChannelRepository::with_clock(now());
        for id in 1..=150 {
            open(&repo, [id; 32]).await;
        }
        let rpc = FakeTempo::default();
        answer(
            &rpc,
            ChannelState {
                settled: 0,
                deposit: 1000,
                close_requested_at: 0,
            },
        );
        let summary = run_supervision_pass(
            &repo,
            &rpc,
            &signer(),
            chain(),
            &SupervisionConfig::default(),
            now(),
        )
        .await;
        assert_eq!((summary.scanned, summary.chain_reads), (150, 2));
        assert_eq!(rpc.eth_calls(), 2);
        assert_eq!(rpc.send_calls(), 0);
        for id in 1..=150 {
            assert_eq!(
                repo.get(&[id; 32])
                    .await
                    .unwrap()
                    .unwrap()
                    .last_chain_read_at,
                Some(now())
            );
        }
    }

    #[tokio::test]
    async fn a_zero_deposit_is_finalized_at_the_chain_settled_amount() {
        let repo = InMemoryChannelRepository::with_clock(now());
        open(&repo, [1; 32]).await;
        let rpc = FakeTempo::default();
        answer(
            &rpc,
            ChannelState {
                settled: 45,
                deposit: 0,
                close_requested_at: 0,
            },
        );
        let summary = run_supervision_pass(
            &repo,
            &rpc,
            &signer(),
            chain(),
            &SupervisionConfig::default(),
            now(),
        )
        .await;
        assert_eq!(summary.finalized_withdrawn, 1);
        let record = repo.get(&[1; 32]).await.unwrap().unwrap();
        assert_eq!(
            (record.state, record.settled_on_chain),
            (ChannelLifecycle::Finalized, 45)
        );
        assert_eq!(record.last_chain_read_at, Some(now()));
        assert_eq!(rpc.send_calls(), 0);
    }

    #[tokio::test]
    async fn a_large_ledger_page_is_split_at_the_abi_batch_limit() {
        let repo = InMemoryChannelRepository::with_clock(now());
        let count = MAX_BATCH_CHANNELS + 1;
        for number in 0..count {
            let mut id = [0; 32];
            id[..2].copy_from_slice(&u16::try_from(number).unwrap().to_be_bytes());
            open(&repo, id).await;
        }
        let rpc = FakeTempo::default();
        answer(
            &rpc,
            ChannelState {
                settled: 0,
                deposit: 1000,
                close_requested_at: 0,
            },
        );
        let cfg = SupervisionConfig {
            page: count,
            ..Default::default()
        };
        let summary = run_supervision_pass(&repo, &rpc, &signer(), chain(), &cfg, now()).await;
        assert_eq!(summary.scanned, count as u64);
        assert_eq!(summary.chain_reads, 2);
        assert_eq!(rpc.eth_calls(), 2);
    }

    #[tokio::test]
    async fn mark_closing_precedes_send_and_capture_excludes_reservations() {
        let repo = Arc::new(InMemoryChannelRepository::with_clock(now()));
        open(&repo, [1; 32]).await;
        spend(&repo, &[1; 32], 80).await;
        reserve(&repo, &[1; 32], 90).await;
        let rpc = OrderedRpc {
            fake: FakeTempo::default(),
            repo: repo.clone(),
            id: [1; 32],
            observed_closing: AtomicUsize::new(0),
            cumulative: 100,
            capture: 80,
            charge_during_build: None,
        };
        answer(&rpc.fake, requested());
        let summary = run_supervision_pass(
            repo.as_ref(),
            &rpc,
            &signer(),
            chain(),
            &SupervisionConfig::default(),
            now(),
        )
        .await;
        assert_eq!(
            (
                summary.close_requested,
                summary.closes_submitted,
                summary.closes_failed
            ),
            (1, 1, 0)
        );
        assert_eq!(rpc.observed_closing.load(Ordering::SeqCst), 1);
        let record = repo.get(&[1; 32]).await.unwrap().unwrap();
        assert_eq!(
            (record.state, record.settled_on_chain),
            (ChannelLifecycle::Finalized, 80)
        );
        assert_eq!(record.pending_close, None);
        assert_eq!(record.close_requested_at, requested().close_requested_at);
        assert!(!record.accepts_vouchers());
    }

    #[tokio::test]
    async fn broadcast_failure_clears_closing_back_to_open() {
        let repo = Arc::new(InMemoryChannelRepository::with_clock(now()));
        open(&repo, [1; 32]).await;
        spend(&repo, &[1; 32], 80).await;
        let rpc = OrderedRpc {
            fake: FakeTempo::default(),
            repo: repo.clone(),
            id: [1; 32],
            observed_closing: AtomicUsize::new(0),
            cumulative: 100,
            capture: 80,
            charge_during_build: None,
        };
        answer(&rpc.fake, requested());
        rpc.fake.set_send_mode(SendMode::Unavailable);
        let summary = run_supervision_pass(
            repo.as_ref(),
            &rpc,
            &signer(),
            chain(),
            &SupervisionConfig::default(),
            now(),
        )
        .await;
        assert_eq!((summary.closes_submitted, summary.closes_failed), (1, 1));
        assert_eq!(rpc.observed_closing.load(Ordering::SeqCst), 1);
        let record = repo.get(&[1; 32]).await.unwrap().unwrap();
        assert_eq!(
            (record.state, record.settled_on_chain),
            (ChannelLifecycle::Open, 0)
        );
        assert_eq!(record.pending_close, None);
        assert!(!record.accepts_vouchers());
    }

    #[tokio::test]
    async fn requested_close_with_no_new_delivered_spend_sends_nothing() {
        let repo = InMemoryChannelRepository::with_clock(now());
        open(&repo, [1; 32]).await;
        spend(&repo, &[1; 32], 60).await;
        reserve(&repo, &[1; 32], 100).await;
        let rpc = FakeTempo::default();
        answer(
            &rpc,
            ChannelState {
                settled: 60,
                ..requested()
            },
        );
        let summary = run_supervision_pass(
            &repo,
            &rpc,
            &signer(),
            chain(),
            &SupervisionConfig::default(),
            now(),
        )
        .await;
        assert_eq!(
            (
                summary.close_requested,
                summary.nothing_to_capture,
                summary.closes_submitted
            ),
            (1, 1, 0)
        );
        assert_eq!(rpc.send_calls(), 0);
        let record = repo.get(&[1; 32]).await.unwrap().unwrap();
        assert_eq!(record.state, ChannelLifecycle::Open);
        assert!(!record.accepts_vouchers());
    }

    #[tokio::test]
    async fn stale_reservations_are_released_once_and_recent_ones_remain() {
        let repo = InMemoryChannelRepository::with_clock(now());
        open(&repo, [1; 32]).await;
        let stale = reserve(&repo, &[1; 32], 60).await;
        reserve(&repo, &[1; 32], 30).await;
        repo.age_reservation(stale, chrono::Duration::minutes(16));
        let rpc = FakeTempo::default();
        answer(
            &rpc,
            ChannelState {
                settled: 0,
                deposit: 1000,
                close_requested_at: 0,
            },
        );
        let summary = run_supervision_pass(
            &repo,
            &rpc,
            &signer(),
            chain(),
            &SupervisionConfig::default(),
            now(),
        )
        .await;
        assert_eq!(summary.stale_reservations_released, 1);
        assert_eq!(repo.reservation_count(), 1);
        let record = repo.get(&[1; 32]).await.unwrap().unwrap();
        assert_eq!((record.spent, record.reserved), (30, 30));
        assert_eq!(
            run_supervision_pass(
                &repo,
                &rpc,
                &signer(),
                chain(),
                &SupervisionConfig::default(),
                now()
            )
            .await
            .stale_reservations_released,
            0
        );
    }

    #[tokio::test]
    async fn failed_chain_read_skips_its_channel_but_later_pages_continue() {
        let repo = InMemoryChannelRepository::with_clock(now());
        open(&repo, [1; 32]).await;
        open(&repo, [2; 32]).await;
        let rpc = FakeTempo::default();
        rpc.set_call_handler(Box::new(|_, data| {
            if data[68..100] == [1; 32] {
                return Err(RpcError::Unavailable);
            }
            Ok(encode_states(&[ChannelState {
                settled: 20,
                deposit: 0,
                close_requested_at: 0,
            }]))
        }));
        let cfg = SupervisionConfig {
            page: 1,
            ..Default::default()
        };
        let summary = run_supervision_pass(&repo, &rpc, &signer(), chain(), &cfg, now()).await;
        assert_eq!(
            (
                summary.scanned,
                summary.chain_reads,
                summary.finalized_withdrawn
            ),
            (2, 2, 1)
        );
        assert_eq!(
            repo.get(&[1; 32])
                .await
                .unwrap()
                .unwrap()
                .last_chain_read_at,
            None
        );
        assert_eq!(
            repo.get(&[2; 32]).await.unwrap().unwrap().state,
            ChannelLifecycle::Finalized
        );
    }

    #[tokio::test]
    async fn malformed_and_wrong_length_batches_do_not_update_any_channel() {
        for bytes in [
            vec![],
            encode_states(&[]),
            encode_states(&[requested(), requested()]),
        ] {
            let repo = InMemoryChannelRepository::with_clock(now());
            open(&repo, [1; 32]).await;
            let rpc = FakeTempo::default();
            rpc.set_call_handler(Box::new(move |_, _| Ok(bytes.clone())));
            let summary = run_supervision_pass(
                &repo,
                &rpc,
                &signer(),
                chain(),
                &SupervisionConfig::default(),
                now(),
            )
            .await;
            assert_eq!(
                (
                    summary.scanned,
                    summary.chain_reads,
                    summary.closes_submitted
                ),
                (1, 1, 0)
            );
            assert_eq!(
                repo.get(&[1; 32])
                    .await
                    .unwrap()
                    .unwrap()
                    .last_chain_read_at,
                None
            );
            assert_eq!(rpc.send_calls(), 0);
        }
    }

    #[tokio::test]
    async fn changed_capture_during_build_is_not_broadcast() {
        let repo = Arc::new(InMemoryChannelRepository::with_clock(now()));
        open(&repo, [1; 32]).await;
        spend(&repo, &[1; 32], 80).await;
        let request = reserve(&repo, &[1; 32], 20).await;
        let rpc = OrderedRpc {
            fake: FakeTempo::default(),
            repo: repo.clone(),
            id: [1; 32],
            observed_closing: AtomicUsize::new(0),
            cumulative: 100,
            capture: 80,
            charge_during_build: Some((request, 20)),
        };
        answer(&rpc.fake, requested());
        let summary = run_supervision_pass(
            repo.as_ref(),
            &rpc,
            &signer(),
            chain(),
            &SupervisionConfig::default(),
            now(),
        )
        .await;
        assert_eq!((summary.closes_submitted, summary.closes_failed), (0, 1));
        assert_eq!(rpc.fake.send_calls(), 0);
        let record = repo.get(&[1; 32]).await.unwrap().unwrap();
        assert_eq!(
            (record.state, record.spent, record.reserved),
            (ChannelLifecycle::Open, 100, 0)
        );
        assert_eq!(record.pending_close, None);
    }

    #[tokio::test]
    async fn a_foreign_chain_is_never_read_or_finalized() {
        let repo = InMemoryChannelRepository::with_clock(now());
        open(&repo, [1; 32]).await;
        let rpc = FakeTempo::default();
        answer(
            &rpc,
            ChannelState {
                settled: 0,
                deposit: 0,
                close_requested_at: 0,
            },
        );
        let foreign = PayeeChain {
            chain_id: chain().chain_id + 1,
            ..chain()
        };
        let summary = run_supervision_pass(
            &repo,
            &rpc,
            &signer(),
            foreign,
            &SupervisionConfig::default(),
            now(),
        )
        .await;
        assert_eq!(
            (
                summary.scanned,
                summary.chain_reads,
                summary.finalized_withdrawn
            ),
            (1, 0, 0)
        );
        assert_eq!(rpc.eth_calls(), 0);
        assert_eq!(
            repo.get(&[1; 32]).await.unwrap().unwrap().state,
            ChannelLifecycle::Open
        );
    }
}
