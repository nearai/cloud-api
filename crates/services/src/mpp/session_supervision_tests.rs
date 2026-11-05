use crate::mpp::abi::{close_calldata, ChannelState, MAX_BATCH_CHANNELS};
use crate::mpp::abi::{uint_word, GET_CHANNEL_STATES_BATCH_SELECTOR};
use crate::mpp::channel_memory::InMemoryChannelRepository;
use crate::mpp::ports::{ChannelLifecycle, ChannelRepository, PendingClose, TempoRpc, TempoSigner};
use crate::mpp::ports::{NewChannel, ReserveOutcome, RpcError, RpcReceipt};
use crate::mpp::session_chain::PayeeChain;
use crate::mpp::session_settlement_tests::TestRepo;
use crate::mpp::session_supervision::*;
use crate::mpp::session_vectors::Vectors;
use crate::mpp::signer::LocalSigner;
use crate::mpp::tempo_tx::{decode_signed, tx_hash};
use crate::mpp::testing::{FakeTempo, SendMode};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
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
        repo.accept_voucher(
            &id,
            cumulative,
            v.fixed("voucher.signature"),
            crate::mpp::ports::VoucherRules {
                min_delta: 0,
                settled_floor_from_chain: true,
            },
        )
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

#[tokio::test]
async fn pending_close_recovery_handles_receipts_expiry_and_hash_mismatch() {
    // Receipt status, seconds past valid_before, expected lifecycle.
    for (receipt, age, expected) in [
        (Some(true), 0, ChannelLifecycle::Finalized),
        (Some(false), 0, ChannelLifecycle::Open),
        (None, 0, ChannelLifecycle::Closing),
        (None, 30, ChannelLifecycle::Closing),
        (None, 31, ChannelLifecycle::Open),
    ] {
        let repo = InMemoryChannelRepository::with_clock(now());
        open(&repo, [1; 32]).await;
        let pending = PendingClose {
            tx_hash: [9; 32],
            cumulative: 100,
            capture: 80,
            valid_before: u64::try_from(now().timestamp()).unwrap(),
        };
        repo.mark_closing(&[1; 32], pending, Utc::now())
            .await
            .unwrap();
        let rpc = FakeTempo::default();
        answer(&rpc, requested());
        if let Some(success) = receipt {
            rpc.mine(RpcReceipt {
                tx_hash: pending.tx_hash,
                success,
                block_number: 1,
                from: signer().address(),
                logs: vec![],
            });
        }
        let summary = run_supervision_pass(
            &repo,
            &rpc,
            &signer(),
            chain(),
            &SupervisionConfig::default(),
            now() + chrono::Duration::seconds(age),
        )
        .await;
        let record = repo.get(&[1; 32]).await.unwrap().unwrap();
        assert_eq!(record.state, expected, "receipt={receipt:?}, age={age}");
        assert_eq!(
            record.pending_close,
            if expected == ChannelLifecycle::Closing {
                Some(pending)
            } else {
                None
            }
        );
        assert_eq!(
            record.settled_on_chain,
            if expected == ChannelLifecycle::Finalized {
                80
            } else {
                0
            }
        );
        assert_eq!(rpc.lookup_calls(), 1);
        assert_eq!(rpc.send_calls(), 0);
        assert_eq!(summary.closes_submitted, 0);
        assert_eq!(
            summary.reconciled_finalized,
            u64::from(receipt == Some(true))
        );
        assert_eq!(
            summary.reconciled_reverted,
            u64::from(receipt == Some(false))
        );
        assert_eq!(
            summary.reconciled_expired,
            u64::from(receipt.is_none() && age > 30)
        );
        assert_eq!(
            summary.reconciliation_pending,
            u64::from(receipt.is_none() && age <= 30)
        );
        assert_eq!(summary.reconciliation_failed, 0);
        assert_eq!(rpc.eth_calls(), 0);
    }
    let repo = InMemoryChannelRepository::with_clock(now());
    open(&repo, [1; 32]).await;
    let pending = PendingClose {
        tx_hash: [9; 32],
        cumulative: 100,
        capture: 80,
        valid_before: 0,
    };
    repo.mark_closing(&[1; 32], pending, Utc::now())
        .await
        .unwrap();
    let rpc = FakeTempo::default();
    answer(&rpc, requested());
    rpc.mined_alias(
        pending.tx_hash,
        RpcReceipt {
            tx_hash: [8; 32],
            success: true,
            block_number: 1,
            from: signer().address(),
            logs: vec![],
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
    assert_eq!(summary.reconciliation_failed, 1);
    assert_eq!(
        repo.get(&[1; 32]).await.unwrap().unwrap().pending_close,
        Some(pending)
    );
    assert_eq!(rpc.lookup_calls(), 1);
}

#[tokio::test]
async fn receipt_read_error_preserves_even_an_expired_pending_close() {
    let repo = InMemoryChannelRepository::with_clock(now());
    open(&repo, [1; 32]).await;
    let pending = PendingClose {
        tx_hash: [9; 32],
        cumulative: 100,
        capture: 80,
        valid_before: 0,
    };
    repo.mark_closing(&[1; 32], pending, Utc::now())
        .await
        .unwrap();
    let rpc = FakeTempo::default();
    rpc.set_lookup_error(Some(RpcError::Unavailable));
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
        (summary.reconciliation_failed, summary.reconciled_expired),
        (1, 0)
    );
    let record = repo.get(&[1; 32]).await.unwrap().unwrap();
    assert_eq!(
        (record.state, record.pending_close),
        (ChannelLifecycle::Closing, Some(pending))
    );
    assert_eq!(rpc.lookup_calls(), 1);
    assert_eq!(rpc.send_calls(), 0);
}

#[tokio::test]
async fn successful_broadcast_failed_finalize_preserves_hash_then_recovers() {
    let inner = Arc::new(InMemoryChannelRepository::with_clock(now()));
    open(&inner, [1; 32]).await;
    spend(&inner, &[1; 32], 80).await;
    let repo = TestRepo::new(&inner);
    repo.fail_finalize.store(true, Ordering::SeqCst);
    let rpc = OrderedRpc {
        fake: FakeTempo::default(),
        repo: inner.clone(),
        id: [1; 32],
        observed_closing: AtomicUsize::new(0),
        cumulative: 100,
        capture: 80,
        charge_during_build: None,
        settled_during_build: None,
    };
    answer(&rpc.fake, requested());
    let first = run_supervision_pass(
        &repo,
        &rpc,
        &signer(),
        chain(),
        &SupervisionConfig::default(),
        now(),
    )
    .await;
    assert_eq!((first.closes_submitted, first.closes_failed), (1, 1));
    assert_eq!(repo.finalize_calls.load(Ordering::SeqCst), 1);
    let record = inner.get(&[1; 32]).await.unwrap().unwrap();
    assert_eq!(record.state, ChannelLifecycle::Closing);
    let pending = record.pending_close.unwrap();
    assert!(
        rpc.get_transaction_receipt(pending.tx_hash)
            .await
            .unwrap()
            .unwrap()
            .success
    );
    let second = run_supervision_pass(
        &repo,
        &rpc,
        &signer(),
        chain(),
        &SupervisionConfig::default(),
        now(),
    )
    .await;
    assert_eq!((second.closes_submitted, second.closes_failed), (0, 0));
    assert_eq!(second.reconciled_finalized, 1);
    let record = inner.get(&[1; 32]).await.unwrap().unwrap();
    assert_eq!(
        (record.state, record.settled_on_chain),
        (ChannelLifecycle::Finalized, 80)
    );
    assert_eq!(record.pending_close, None);
    assert_eq!(repo.finalize_calls.load(Ordering::SeqCst), 2);
    assert_eq!(rpc.fake.send_calls(), 1);
}

#[tokio::test]
async fn settled_at_capture_during_build_defers_close() {
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
        settled_during_build: Some(80),
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
    let record = repo.get(&[1; 32]).await.unwrap().unwrap();
    assert_eq!((record.delivered(), record.settled_on_chain), (80, 80));
    assert_eq!(
        (record.state, record.pending_close),
        (ChannelLifecycle::Open, None)
    );
    assert_eq!(rpc.fake.send_calls(), 0);
}

#[tokio::test]
async fn revert_clears_pending_but_rejected_without_receipt_stays_pending() {
    for reverted in [false, true] {
        let repo = InMemoryChannelRepository::with_clock(now());
        open(&repo, [1; 32]).await;
        spend(&repo, &[1; 32], 80).await;
        let rpc = FakeTempo::default();
        answer(&rpc, requested());
        rpc.set_send_mode(SendMode::Reject);
        if reverted {
            let record = repo.get(&[1; 32]).await.unwrap().unwrap();
            let tx = crate::mpp::session_chain::build_payee_tx(
                chain(),
                &signer(),
                &rpc,
                close_calldata(&record.descriptor, 100, 80, &record.highest_signature).unwrap(),
                u64::try_from(now().timestamp()).unwrap(),
            )
            .await
            .unwrap();
            rpc.mine(RpcReceipt {
                tx_hash: tx.tx_hash,
                success: false,
                block_number: 1,
                from: signer().address(),
                logs: vec![],
            });
        }
        let summary = run_supervision_pass(
            &repo,
            &rpc,
            &signer(),
            chain(),
            &SupervisionConfig::default(),
            now(),
        )
        .await;
        assert_eq!((summary.closes_submitted, summary.closes_failed), (1, 1));
        let record = repo.get(&[1; 32]).await.unwrap().unwrap();
        if reverted {
            assert_eq!(
                (record.state, record.pending_close),
                (ChannelLifecycle::Open, None)
            );
        } else {
            assert_eq!(record.state, ChannelLifecycle::Closing);
            assert!(record.pending_close.is_some());
        }
        assert_eq!(rpc.send_calls(), 1);
        assert_eq!(rpc.lookup_calls(), 1);
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
    settled_during_build: Option<u128>,
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
        if let Some(settled) = self.settled_during_build {
            self.repo
                .record_chain_state(
                    &self.id,
                    ChannelState {
                        settled,
                        ..requested()
                    },
                    now(),
                )
                .await
                .unwrap();
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
        settled_during_build: None,
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
async fn ambiguous_broadcast_keeps_pending_and_next_pass_reconciles() {
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
        settled_during_build: None,
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
        (ChannelLifecycle::Closing, 0)
    );
    assert!(record.pending_close.is_some());
    assert!(!record.accepts_vouchers());
    let second = run_supervision_pass(
        repo.as_ref(),
        &rpc,
        &signer(),
        chain(),
        &SupervisionConfig::default(),
        now(),
    )
    .await;
    assert_eq!(second.closes_submitted, 0);
    assert_eq!(
        repo.get(&[1; 32]).await.unwrap().unwrap().state,
        ChannelLifecycle::Finalized
    );
    assert_eq!(rpc.fake.send_calls(), 1);
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
        settled_during_build: None,
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
