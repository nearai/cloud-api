use crate::mpp::abi::{get_channel_state_calldata, settle_calldata, uint_word, ChannelState};
use crate::mpp::channel_memory::InMemoryChannelRepository;
use crate::mpp::ports::{
    ChannelLifecycle, ChannelRecord, ChannelRepository, RetainedVoucher, TempoRpc, TempoSigner,
};
use crate::mpp::ports::{NewChannel, PendingClose, ReserveOutcome, RpcError, RpcReceipt};
use crate::mpp::session_chain::{build_payee_tx, PayeeChain};
use crate::mpp::session_settlement::*;
use crate::mpp::session_vectors::Vectors;
use crate::mpp::signer::LocalSigner;
use crate::mpp::testing::{FakeTempo, SendMode};
use chrono::{DateTime, Utc};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use uuid::Uuid;

struct Harness {
    repo: InMemoryChannelRepository,
    rpc: FakeTempo,
    signer: LocalSigner,
    chain: PayeeChain,
    now: DateTime<Utc>,
    id: [u8; 32],
}

fn chain_answer(rpc: &FakeTempo, id: [u8; 32], settled: u128) {
    rpc.set_call_handler(Box::new(move |to, data| {
        assert_eq!(to, Vectors::load().escrow());
        assert_eq!(data, get_channel_state_calldata(&id));
        let mut bytes = uint_word(settled).to_vec();
        bytes.extend(uint_word(1000));
        bytes.extend(uint_word(0));
        Ok(bytes)
    }));
}

#[tokio::test]
async fn live_highest_voucher_wins_with_its_own_signature() {
    let h = Harness::new().await;
    h.spend(200).await;
    let mut record = h.record().await;
    record.highest_signature = [0x42; 65];
    let expected = Some(RetainedVoucher {
        cumulative: 200,
        signature: [0x42; 65],
    });
    assert_eq!(record.delivered(), record.highest_cumulative);
    assert_eq!(record.settleable_voucher(), expected);
    record.retained_vouchers.clear();
    assert_eq!(record.settleable_voucher(), expected);
}

async fn assert_post_claim_abort(action: ClaimAction) {
    let h = Harness::new().await;
    h.spend(100).await;
    let mut repo = TestRepo::new(&h.repo);
    repo.claim_action = action;
    let summary = run_settlement_pass(
        &repo,
        &h.rpc,
        &h.signer,
        h.chain,
        &SettlementConfig::default(),
        h.now,
    )
    .await;
    assert_eq!((summary.due, summary.settled, summary.failed), (1, 0, 0));
    assert_eq!(
        summary.no_settleable_voucher,
        u64::from(matches!(action, ClaimAction::NoVoucher))
    );
    assert_eq!(repo.claim_calls.load(Ordering::SeqCst), 1);
    assert_eq!(repo.release_calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.repo.lease_until(&h.id), None);
    assert_eq!(h.rpc.send_calls(), 0);
    assert_eq!(h.rpc.eth_calls(), 0);
}

#[tokio::test]
async fn post_claim_closing_releases_the_held_lease() {
    assert_post_claim_abort(ClaimAction::Closing).await;
}

#[tokio::test]
async fn post_claim_not_due_releases_the_held_lease() {
    assert_post_claim_abort(ClaimAction::NotDue).await;
}

#[tokio::test]
async fn post_claim_no_voucher_releases_the_held_lease() {
    assert_post_claim_abort(ClaimAction::NoVoucher).await;
}
#[tokio::test]
async fn landed_settlement_with_failed_record_is_reconciled_before_retry() {
    let h = Harness::new().await;
    h.spend(100).await;
    h.mine(100, true).await;
    let repo = TestRepo::new(&h.repo);
    repo.fail_settlement.store(true, Ordering::SeqCst);
    let first = run_settlement_pass(
        &repo,
        &h.rpc,
        &h.signer,
        h.chain,
        &SettlementConfig::default(),
        h.now,
    )
    .await;
    assert_eq!((first.settled, first.failed), (0, 1));
    assert_eq!(repo.settlement_calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.rpc.send_calls(), 1);
    assert_eq!(h.record().await.settled_on_chain, 0);
    assert_eq!(h.repo.lease_until(&h.id), None);
    chain_answer(&h.rpc, h.id, 100);
    let second = run_settlement_pass(
        &repo,
        &h.rpc,
        &h.signer,
        h.chain,
        &SettlementConfig::default(),
        h.now,
    )
    .await;
    assert_eq!(
        (second.settled, second.failed, second.no_settleable_voucher),
        (0, 0, 1)
    );
    assert_eq!(h.record().await.settled_on_chain, 100);
    assert_eq!(h.rpc.send_calls(), 1);
    assert_eq!(h.rpc.eth_calls(), 2);
    assert_eq!(h.repo.lease_until(&h.id), None);
}

#[tokio::test]
async fn failed_reconciliation_never_broadcasts_and_releases_lease() {
    for malformed in [false, true] {
        let h = Harness::new().await;
        h.spend(100).await;
        h.mine(100, true).await;
        h.rpc.set_call_handler(Box::new(move |_, _| {
            if malformed {
                Ok(vec![])
            } else {
                Err(RpcError::Unavailable)
            }
        }));
        let summary = h.run(&SettlementConfig::default()).await;
        assert_eq!((summary.failed, summary.settled), (1, 0));
        assert_eq!(h.rpc.send_calls(), 0);
        assert_eq!(h.repo.lease_until(&h.id), None);
    }
}

impl Harness {
    async fn new() -> Self {
        let v = Vectors::load();
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let repo = InMemoryChannelRepository::with_clock(now);
        let id = v.fixed("channelId");
        repo.open_or_refresh(NewChannel {
            channel_id: id,
            chain_id: v.chain_id(),
            escrow: v.escrow(),
            descriptor: v.descriptor(),
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
        let rpc = FakeTempo::default();
        chain_answer(&rpc, id, 0);
        Self {
            repo,
            rpc,
            signer: LocalSigner::new(v.secret_key("payeeKey")),
            chain: PayeeChain {
                chain_id: v.chain_id(),
                escrow: v.escrow(),
                fee_token: v.fixed("token"),
            },
            now,
            id,
        }
    }

    async fn reserve(&self, amount: u128) -> Uuid {
        let request = Uuid::new_v4();
        assert!(matches!(
            self.repo.reserve(&self.id, request, amount).await.unwrap(),
            ReserveOutcome::Reserved(_)
        ));
        request
    }

    async fn spend(&self, amount: u128) {
        let request = self.reserve(amount).await;
        self.repo.charge(request, amount, 1).await.unwrap();
    }

    async fn record(&self) -> ChannelRecord {
        self.repo.get(&self.id).await.unwrap().unwrap()
    }

    async fn mine(&self, cumulative: u128, success: bool) {
        let record = self.record().await;
        let data =
            settle_calldata(&record.descriptor, cumulative, &record.highest_signature).unwrap();
        let tx = build_payee_tx(
            self.chain,
            &self.signer,
            &self.rpc,
            data,
            self.now.timestamp() as u64,
        )
        .await
        .unwrap();
        self.rpc.mine(RpcReceipt {
            tx_hash: tx.tx_hash,
            block_number: 1,
            success,
            from: self.signer.address(),
            logs: vec![],
        });
    }

    async fn run(&self, cfg: &SettlementConfig) -> SettlementSummary {
        run_settlement_pass(
            &self.repo,
            &self.rpc,
            &self.signer,
            self.chain,
            cfg,
            self.now,
        )
        .await
    }
}

#[derive(Clone, Copy, Default)]
enum ClaimAction {
    #[default]
    None,
    Charge(Uuid),
    Closing,
    NotDue,
    NoVoucher,
}

struct CloseDuringBuild<'a> {
    h: &'a Harness,
}

#[async_trait::async_trait]
impl TempoRpc for CloseDuringBuild<'_> {
    async fn send_raw_transaction_sync(&self, raw: &[u8]) -> Result<RpcReceipt, RpcError> {
        self.h.rpc.send_raw_transaction_sync(raw).await
    }
    async fn get_transaction_receipt(
        &self,
        hash: [u8; 32],
    ) -> Result<Option<RpcReceipt>, RpcError> {
        self.h.rpc.get_transaction_receipt(hash).await
    }
    async fn get_block_timestamp(&self, block: u64) -> Result<Option<u64>, RpcError> {
        self.h.rpc.get_block_timestamp(block).await
    }
    async fn call(&self, to: [u8; 20], data: &[u8]) -> Result<Vec<u8>, RpcError> {
        self.h.rpc.call(to, data).await
    }
    async fn gas_price(&self) -> Result<u128, RpcError> {
        assert!(self.h.repo.lease_until(&self.h.id).is_some());
        assert!(matches!(
            self.h
                .repo
                .mark_closing(
                    &self.h.id,
                    PendingClose {
                        tx_hash: [1; 32],
                        cumulative: 100,
                        capture: 100,
                        valid_before: 1_800_000_025,
                    }
                )
                .await
                .unwrap(),
            crate::mpp::ports::CloseTransition::Marked(_)
        ));
        self.h.rpc.gas_price().await
    }
}

#[tokio::test]
async fn close_during_build_aborts_settlement_before_broadcast() {
    let h = Harness::new().await;
    h.spend(100).await;
    h.mine(100, true).await;
    let summary = run_settlement_pass(
        &h.repo,
        &CloseDuringBuild { h: &h },
        &h.signer,
        h.chain,
        &SettlementConfig::default(),
        h.now,
    )
    .await;
    assert_eq!((summary.settled, summary.failed), (0, 0));
    assert_eq!(h.record().await.state, ChannelLifecycle::Closing);
    assert_eq!(h.rpc.send_calls(), 0);
    assert_eq!(h.repo.lease_until(&h.id), None);
}

// Faults occur at the repository boundary while the real in-memory ledger keeps
// its lifecycle, reservations and lease state.
pub(super) struct TestRepo<'a> {
    inner: &'a InMemoryChannelRepository,
    claim_action: ClaimAction,
    pub(super) fail_finalize: AtomicBool,
    fail_settlement: AtomicBool,
    pub(super) finalize_calls: AtomicUsize,
    settlement_calls: AtomicUsize,
    claim_calls: AtomicUsize,
    release_calls: AtomicUsize,
}

impl<'a> TestRepo<'a> {
    pub(super) fn new(inner: &'a InMemoryChannelRepository) -> Self {
        Self {
            inner,
            claim_action: ClaimAction::None,
            fail_finalize: AtomicBool::new(false),
            fail_settlement: AtomicBool::new(false),
            finalize_calls: AtomicUsize::new(0),
            settlement_calls: AtomicUsize::new(0),
            claim_calls: AtomicUsize::new(0),
            release_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ChannelRepository for TestRepo<'_> {
    async fn open_or_refresh(
        &self,
        channel: NewChannel,
    ) -> Result<ChannelRecord, crate::common::RepositoryError> {
        self.inner.open_or_refresh(channel).await
    }
    async fn get(
        &self,
        channel_id: &[u8; 32],
    ) -> Result<Option<ChannelRecord>, crate::common::RepositoryError> {
        self.inner.get(channel_id).await
    }
    async fn accept_voucher(
        &self,
        channel_id: &[u8; 32],
        cumulative: u128,
        signature: [u8; 65],
    ) -> Result<crate::mpp::ports::VoucherOutcome, crate::common::RepositoryError> {
        self.inner
            .accept_voucher(channel_id, cumulative, signature)
            .await
    }
    async fn reserve(
        &self,
        channel_id: &[u8; 32],
        request_id: Uuid,
        amount: u128,
    ) -> Result<ReserveOutcome, crate::common::RepositoryError> {
        self.inner.reserve(channel_id, request_id, amount).await
    }
    async fn charge(
        &self,
        request_id: Uuid,
        actual: u128,
        units: u64,
    ) -> Result<crate::mpp::ports::ChargeOutcome, crate::common::RepositoryError> {
        self.inner.charge(request_id, actual, units).await
    }
    async fn release(&self, request_id: Uuid) -> Result<bool, crate::common::RepositoryError> {
        self.inner.release(request_id).await
    }
    async fn record_chain_state(
        &self,
        channel_id: &[u8; 32],
        state: crate::mpp::abi::ChannelState,
        read_at: DateTime<Utc>,
    ) -> Result<Option<ChannelRecord>, crate::common::RepositoryError> {
        self.inner
            .record_chain_state(channel_id, state, read_at)
            .await
    }
    async fn mark_closing(
        &self,
        channel_id: &[u8; 32],
        pending: PendingClose,
    ) -> Result<crate::mpp::ports::CloseTransition, crate::common::RepositoryError> {
        self.inner.mark_closing(channel_id, pending).await
    }
    async fn finalize(
        &self,
        channel_id: &[u8; 32],
        settled_on_chain: u128,
    ) -> Result<(), crate::common::RepositoryError> {
        self.finalize_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_finalize.swap(false, Ordering::SeqCst) {
            return Err(crate::common::RepositoryError::QueryTimeout);
        }
        self.inner.finalize(channel_id, settled_on_chain).await
    }
    async fn clear_closing(
        &self,
        channel_id: &[u8; 32],
    ) -> Result<(), crate::common::RepositoryError> {
        self.inner.clear_closing(channel_id).await
    }
    async fn record_settlement(
        &self,
        channel_id: &[u8; 32],
        cumulative: u128,
        at: DateTime<Utc>,
    ) -> Result<(), crate::common::RepositoryError> {
        self.settlement_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_settlement.swap(false, Ordering::SeqCst) {
            return Err(crate::common::RepositoryError::QueryTimeout);
        }
        self.inner
            .record_settlement(channel_id, cumulative, at)
            .await
    }
    async fn release_settlement_lease(
        &self,
        channel_id: &[u8; 32],
    ) -> Result<(), crate::common::RepositoryError> {
        self.release_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.release_settlement_lease(channel_id).await
    }
    async fn list_active(
        &self,
        after: Option<[u8; 32]>,
        limit: usize,
    ) -> Result<Vec<ChannelRecord>, crate::common::RepositoryError> {
        self.inner.list_active(after, limit).await
    }
    async fn release_stale_reservations(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<u64, crate::common::RepositoryError> {
        self.inner.release_stale_reservations(cutoff).await
    }
    async fn purge_finalized(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<u64, crate::common::RepositoryError> {
        self.inner.purge_finalized(cutoff).await
    }
    async fn claim_settlement_lease(
        &self,
        channel_id: &[u8; 32],
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Option<ChannelRecord>, crate::common::RepositoryError> {
        self.claim_calls.fetch_add(1, Ordering::SeqCst);
        let claimed = self
            .inner
            .claim_settlement_lease(channel_id, now, until)
            .await?;
        if claimed.is_none() {
            return Ok(None);
        }
        match self.claim_action {
            ClaimAction::None => {}
            ClaimAction::Charge(request) => {
                self.inner.charge(request, 60, 1).await?;
            }
            ClaimAction::Closing => {
                self.inner
                    .mark_closing(
                        channel_id,
                        PendingClose {
                            tx_hash: [1; 32],
                            cumulative: 100,
                            capture: 100,
                            valid_before: 1_800_000_025,
                        },
                    )
                    .await?;
            }
            ClaimAction::NotDue => {
                self.inner.record_settlement(channel_id, 0, now).await?;
                self.inner
                    .claim_settlement_lease(channel_id, now, until)
                    .await?;
            }
            ClaimAction::NoVoucher => {
                self.inner
                    .record_chain_state(
                        channel_id,
                        ChannelState {
                            settled: 100,
                            deposit: 1000,
                            close_requested_at: 0,
                        },
                        now,
                    )
                    .await?;
            }
        }
        assert_eq!(self.inner.lease_until(channel_id), Some(until));
        self.inner.get(channel_id).await
    }
}

#[tokio::test]
async fn voucher_is_selected_again_from_the_leased_record() {
    let h = Harness::new().await;
    h.spend(80).await;
    let request = h.reserve(90).await;
    assert_eq!(
        h.record().await.settleable_voucher().unwrap().cumulative,
        60
    );
    h.mine(100, true).await;
    let mut repo = TestRepo::new(&h.repo);
    repo.claim_action = ClaimAction::Charge(request);
    let summary = run_settlement_pass(
        &repo,
        &h.rpc,
        &h.signer,
        h.chain,
        &SettlementConfig::default(),
        h.now,
    )
    .await;
    assert_eq!((summary.settled, summary.failed), (1, 0));
    let record = h.record().await;
    assert_eq!((record.delivered(), record.settled_on_chain), (140, 100));
    assert_eq!(h.rpc.send_calls(), 1);
}

#[tokio::test]
async fn due_by_amount_at_the_exact_threshold() {
    let h = Harness::new().await;
    h.repo.record_settlement(&h.id, 0, h.now).await.unwrap();
    h.spend(100).await;
    h.mine(100, true).await;
    let summary = h
        .run(&SettlementConfig {
            amount: 100,
            ..Default::default()
        })
        .await;
    assert_eq!(
        (
            summary.considered,
            summary.due,
            summary.settled,
            summary.failed
        ),
        (1, 1, 1, 0)
    );
}

#[tokio::test]
async fn due_by_interval_at_the_exact_boundary() {
    let h = Harness::new().await;
    h.spend(100).await;
    h.repo
        .record_settlement(&h.id, 0, h.now - chrono::Duration::hours(1))
        .await
        .unwrap();
    h.mine(100, true).await;
    let summary = h.run(&SettlementConfig::default()).await;
    assert_eq!((summary.due, summary.settled), (1, 1));
}

#[tokio::test]
async fn no_previous_settlement_is_immediately_due() {
    let h = Harness::new().await;
    h.spend(60).await;
    h.mine(60, true).await;
    assert_eq!(h.run(&SettlementConfig::default()).await.settled, 1);
}

#[tokio::test]
async fn recent_below_threshold_channel_is_not_due() {
    let h = Harness::new().await;
    h.repo
        .record_settlement(&h.id, 0, h.now - chrono::Duration::seconds(3599))
        .await
        .unwrap();
    h.spend(100).await;
    let summary = h
        .run(&SettlementConfig {
            amount: 101,
            ..Default::default()
        })
        .await;
    assert_eq!(
        (summary.considered, summary.due, summary.settled),
        (1, 0, 0)
    );
    assert_eq!(h.rpc.send_calls(), 0);
    assert_eq!(h.repo.lease_until(&h.id), None);
}

#[tokio::test]
async fn closing_and_finalized_channels_are_skipped() {
    let h = Harness::new().await;
    h.spend(100).await;
    h.repo
        .mark_closing(
            &h.id,
            PendingClose {
                tx_hash: [1; 32],
                cumulative: 100,
                capture: 100,
                valid_before: 1_800_000_025,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        h.run(&SettlementConfig::default()).await,
        SettlementSummary::default()
    );
    assert_eq!(h.record().await.state, ChannelLifecycle::Closing);
    h.repo.finalize(&h.id, 100).await.unwrap();
    assert_eq!(
        h.run(&SettlementConfig::default()).await,
        SettlementSummary::default()
    );
    assert_eq!(h.rpc.send_calls(), 0);
}

#[tokio::test]
async fn settlement_never_captures_reserved_spend() {
    let h = Harness::new().await;
    h.spend(80).await;
    h.reserve(90).await;
    // The retained 100 voucher is below spent=170 but above delivered=80.
    // Only the exact transaction settling 60 has a successful fake receipt.
    h.mine(60, true).await;
    let summary = h.run(&SettlementConfig::default()).await;
    assert_eq!((summary.settled, summary.failed), (1, 0));
    let record = h.record().await;
    assert_eq!(
        (record.settled_on_chain, record.spent, record.reserved),
        (60, 170, 90)
    );
    assert_eq!(
        record
            .retained_vouchers
            .iter()
            .map(|v| v.cumulative)
            .collect::<Vec<_>>(),
        vec![100]
    );
    assert_eq!(h.rpc.send_calls(), 1);
}

#[tokio::test]
async fn no_voucher_at_or_below_delivered_means_no_transaction() {
    let h = Harness::new().await;
    h.reserve(100).await;
    let summary = h.run(&SettlementConfig::default()).await;
    assert_eq!(
        (summary.due, summary.no_settleable_voucher, summary.settled),
        (1, 1, 0)
    );
    assert_eq!(h.rpc.send_calls(), 0);
    assert_eq!(h.repo.lease_until(&h.id), None);
}

#[tokio::test]
async fn an_existing_lease_is_left_untouched() {
    let h = Harness::new().await;
    h.spend(100).await;
    let until = h.now + chrono::Duration::seconds(60);
    h.repo
        .claim_settlement_lease(&h.id, h.now, until)
        .await
        .unwrap()
        .unwrap();
    let summary = h.run(&SettlementConfig::default()).await;
    assert_eq!((summary.lease_busy, summary.settled), (1, 0));
    assert_eq!(h.repo.lease_until(&h.id), Some(until));
    assert_eq!(h.rpc.send_calls(), 0);
}

#[tokio::test]
async fn gas_broadcast_and_revert_failures_release_the_lease() {
    for failure in 0..3 {
        let h = Harness::new().await;
        h.spend(100).await;
        match failure {
            0 => h.rpc.set_gas_price(Err(RpcError::Unavailable)),
            1 => h.rpc.set_send_mode(SendMode::Unavailable),
            _ => h.mine(100, false).await,
        }
        let summary = h.run(&SettlementConfig::default()).await;
        assert_eq!((summary.failed, summary.settled), (1, 0));
        assert_eq!(h.repo.lease_until(&h.id), None);
        let record = h.record().await;
        assert_eq!(record.settled_on_chain, 0);
        assert_eq!(record.last_settlement_at, None);
        assert_eq!(record.retained_vouchers.len(), 2);
        h.rpc.set_gas_price(Ok(600_000_001));
        h.rpc.set_send_mode(SendMode::Normal);
        h.mine(100, true).await;
        assert_eq!(h.run(&SettlementConfig::default()).await.settled, 1);
    }
}

#[tokio::test]
async fn success_records_schedule_and_drops_captured_vouchers() {
    let h = Harness::new().await;
    h.spend(100).await;
    h.mine(100, true).await;
    assert_eq!(h.run(&SettlementConfig::default()).await.settled, 1);
    let record = h.record().await;
    assert_eq!(record.settled_on_chain, 100);
    assert_eq!(record.last_settlement_at, Some(h.now));
    assert_eq!(record.last_settlement_spent, 100);
    assert!(record.retained_vouchers.is_empty());
    assert_eq!(record.highest_cumulative, 200);
    assert_eq!(h.repo.lease_until(&h.id), None);
    assert_eq!(h.run(&SettlementConfig::default()).await.due, 0);
    assert_eq!(h.rpc.send_calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_hanging_broadcast_releases_the_lease_before_its_deadline() {
    let h = Harness::new().await;
    h.spend(100).await;
    h.rpc.set_send_mode(SendMode::Hang);
    let started = tokio::time::Instant::now();
    let lease = Duration::from_millis(400);
    let summary = h
        .run(&SettlementConfig {
            lease,
            ..Default::default()
        })
        .await;
    assert!(started.elapsed() >= lease / 2);
    assert!(started.elapsed() < lease, "attempt exceeded the lease");
    assert_eq!((summary.failed, summary.settled), (1, 0));
    assert_eq!(h.repo.lease_until(&h.id), None);
    assert_eq!(h.rpc.send_calls(), 1);
}

#[tokio::test]
async fn a_different_chain_is_not_claimed_or_broadcast() {
    let h = Harness::new().await;
    h.spend(100).await;
    let chain = PayeeChain {
        chain_id: h.chain.chain_id + 1,
        ..h.chain
    };
    let summary = run_settlement_pass(
        &h.repo,
        &h.rpc,
        &h.signer,
        chain,
        &SettlementConfig::default(),
        h.now,
    )
    .await;
    assert_eq!((summary.failed, summary.settled), (1, 0));
    assert_eq!(h.rpc.send_calls(), 0);
    assert_eq!(h.repo.lease_until(&h.id), None);
}
