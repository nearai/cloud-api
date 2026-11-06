use super::*;

struct PausedBatchRpc {
    fake: FakeTempo,
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
    calls: AtomicUsize,
}

#[async_trait]
impl TempoRpc for PausedBatchRpc {
    async fn send_raw_transaction_sync(&self, raw: &[u8]) -> Result<RpcReceipt, RpcError> {
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
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            self.resume.notified().await;
        }
        self.fake.call(to, data).await
    }
    async fn gas_price(&self) -> Result<u128, RpcError> {
        self.fake.gas_price().await
    }
}

#[tokio::test]
async fn supervision_claims_only_the_batch_whose_rpc_is_starting() {
    let repo = Arc::new(InMemoryChannelRepository::with_clock(now()));
    for index in 1_u16..=257 {
        let mut id = [0; 32];
        id[..2].copy_from_slice(&index.to_be_bytes());
        open(&repo, id).await;
    }
    let rpc = Arc::new(PausedBatchRpc {
        fake: FakeTempo::default(),
        entered: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
        calls: AtomicUsize::new(0),
    });
    answer(
        &rpc.fake,
        ChannelState {
            close_requested_at: 0,
            ..requested()
        },
    );
    let pass = {
        let repo = repo.clone();
        let rpc = rpc.clone();
        tokio::spawn(async move {
            run_supervision_pass(
                repo.as_ref(),
                rpc.as_ref(),
                &signer(),
                chain(),
                &SupervisionConfig {
                    page: 300,
                    ..Default::default()
                },
                now(),
            )
            .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), rpc.entered.notified())
        .await
        .unwrap();
    assert_eq!(repo.chain_read_claim_count(), MAX_BATCH_CHANNELS);
    let mut later_id = [0; 32];
    later_id[..2].copy_from_slice(&257_u16.to_be_bytes());
    assert!(repo
        .get(&later_id)
        .await
        .unwrap()
        .unwrap()
        .last_chain_read_at
        .is_none());
    rpc.resume.notify_one();
    let summary = pass.await.unwrap();
    assert_eq!(summary.chain_reads, 2);
    assert_eq!(repo.chain_read_claim_count(), 257);
}

#[tokio::test]
async fn fresh_chain_observation_does_not_defer_a_due_payee_close() {
    for reserved in [false, true] {
        let repo = Arc::new(InMemoryChannelRepository::with_clock(now()));
        let v = Vectors::load();
        let mut descriptor = v.descriptor();
        descriptor.salt = [1; 32];
        repo.open_or_refresh(NewChannel {
            channel_id: [1; 32],
            chain_id: v.chain_id(),
            escrow: v.escrow(),
            descriptor,
            deposit: 1000,
            settled_on_chain: 0,
            highest_cumulative: 200,
            highest_signature: [200; 65],
        })
        .await
        .unwrap();
        spend(&repo, &[1; 32], 80).await;
        if reserved {
            reserve(&repo, &[1; 32], 90).await;
        }
        let at = now() + chrono::Duration::seconds(590);
        repo.record_chain_state(&[1; 32], requested(), at)
            .await
            .unwrap();
        let rpc = OrderedRpc {
            fake: FakeTempo::default(),
            repo: repo.clone(),
            id: [1; 32],
            observed_closing: AtomicUsize::new(0),
            cumulative: 200,
            capture: if reserved { 170 } else { 80 },
            charge_during_build: None,
            settled_during_build: None,
        };
        let summary = run_supervision_pass(
            repo.as_ref(),
            &rpc,
            &signer(),
            chain(),
            &Default::default(),
            at,
        )
        .await;
        assert_eq!(
            (summary.closes_submitted, summary.closes_failed),
            (1, 0),
            "reserved={reserved}"
        );
        assert_eq!(rpc.fake.eth_calls(), 0);
        assert_eq!(repo.chain_read_claim_count(), 0);
        let row = repo.get(&[1; 32]).await.unwrap().unwrap();
        assert_eq!(row.state, ChannelLifecycle::Finalized);
        assert_eq!(row.last_chain_read_at, Some(at));
    }
}

#[tokio::test]
async fn fresh_close_request_counts_live_reservation_deferral() {
    let repo = InMemoryChannelRepository::with_clock(now());
    open(&repo, [1; 32]).await;
    spend(&repo, &[1; 32], 80).await;
    reserve(&repo, &[1; 32], 90).await;
    repo.record_chain_state(&[1; 32], requested(), now())
        .await
        .unwrap();
    let rpc = FakeTempo::default();
    let summary =
        run_supervision_pass(&repo, &rpc, &signer(), chain(), &Default::default(), now()).await;
    assert_eq!(
        (summary.reservation_deferrals, summary.close_requested),
        (1, 1)
    );
    assert_eq!((rpc.eth_calls(), rpc.send_calls()), (0, 0));
    assert_eq!(repo.chain_read_claim_count(), 0);
    assert_eq!(
        repo.get(&[1; 32])
            .await
            .unwrap()
            .unwrap()
            .last_chain_read_at,
        Some(now())
    );
}

#[tokio::test]
async fn live_reservation_defers_a_requested_close_before_withdrawal_deadline() {
    let repo = InMemoryChannelRepository::with_clock(now());
    open(&repo, [1; 32]).await;
    spend(&repo, &[1; 32], 80).await;
    reserve(&repo, &[1; 32], 90).await;
    let rpc = FakeTempo::default();
    answer(&rpc, requested());
    let result =
        run_supervision_pass(&repo, &rpc, &signer(), chain(), &Default::default(), now()).await;
    assert_eq!(result.closes_submitted, 0);
    assert_eq!(result.reservation_deferrals, 1);
    assert_eq!(rpc.send_calls(), 0);
    assert_eq!(
        repo.get(&[1; 32]).await.unwrap().unwrap().state,
        ChannelLifecycle::Open
    );
}

#[tokio::test]
async fn fresh_supervision_skips_rpc_and_preserves_observation_time() {
    let repo = InMemoryChannelRepository::with_clock(now());
    open(&repo, [1; 32]).await;
    let state = ChannelState {
        close_requested_at: 0,
        ..requested()
    };
    repo.record_chain_state(&[1; 32], state, now())
        .await
        .unwrap();
    let rpc = FakeTempo::default();
    answer(&rpc, state);
    let result = run_supervision_pass(
        &repo,
        &rpc,
        &signer(),
        chain(),
        &Default::default(),
        now() + chrono::Duration::seconds(1),
    )
    .await;
    assert_eq!(result.chain_reads, 0);
    assert_eq!(rpc.eth_calls(), 0);
    assert_eq!(repo.chain_read_claim_count(), 0);
    assert_eq!(
        repo.get(&[1; 32])
            .await
            .unwrap()
            .unwrap()
            .last_chain_read_at,
        Some(now())
    );
}

#[tokio::test]
async fn supervision_refreshes_at_its_configured_interval() {
    let repo = InMemoryChannelRepository::with_clock(now());
    open(&repo, [1; 32]).await;
    let state = ChannelState {
        close_requested_at: 0,
        ..requested()
    };
    repo.record_chain_state(&[1; 32], state, now())
        .await
        .unwrap();
    let rpc = FakeTempo::default();
    answer(&rpc, state);
    let cfg = SupervisionConfig {
        every: std::time::Duration::from_secs(5),
        ..Default::default()
    };
    let result = run_supervision_pass(
        &repo,
        &rpc,
        &signer(),
        chain(),
        &cfg,
        now() + chrono::Duration::seconds(5),
    )
    .await;
    assert_eq!(result.chain_reads, 1);
    assert_eq!(rpc.eth_calls(), 1);
}

#[tokio::test]
async fn expired_close_with_missing_receipt_requires_a_successful_state_read() {
    let repo = InMemoryChannelRepository::with_clock(now());
    open(&repo, [1; 32]).await;
    let pending = PendingClose {
        tx_hash: [9; 32],
        cumulative: 100,
        capture: 80,
        valid_before: 0,
    };
    repo.mark_closing(&[1; 32], pending, now()).await.unwrap();
    let rpc = FakeTempo::default();
    let result =
        run_supervision_pass(&repo, &rpc, &signer(), chain(), &Default::default(), now()).await;
    assert_eq!(result.reconciled_expired, 0);
    assert_eq!(
        repo.get(&[1; 32]).await.unwrap().unwrap().pending_close,
        Some(pending)
    );
}

#[test]
fn stale_reservation_cutoff_outlives_streams() {
    assert_eq!(
        SupervisionConfig::default().stale_reservation_after,
        std::time::Duration::from_secs(2 * 60 * 60)
    );
}

struct SlowRpc {
    fake: FakeTempo,
    active: AtomicUsize,
    maximum: AtomicUsize,
}

impl SlowRpc {
    async fn pause(&self) {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl TempoRpc for SlowRpc {
    async fn send_raw_transaction_sync(&self, raw: &[u8]) -> Result<RpcReceipt, RpcError> {
        self.pause().await;
        Ok(RpcReceipt {
            tx_hash: tx_hash(raw),
            block_number: 1,
            success: true,
            from: signer().address(),
            logs: vec![],
        })
    }
    async fn get_transaction_receipt(&self, _: [u8; 32]) -> Result<Option<RpcReceipt>, RpcError> {
        self.pause().await;
        Ok(None)
    }
    async fn get_block_timestamp(&self, number: u64) -> Result<Option<u64>, RpcError> {
        self.fake.get_block_timestamp(number).await
    }
    async fn call(&self, to: [u8; 20], data: &[u8]) -> Result<Vec<u8>, RpcError> {
        self.fake.call(to, data).await
    }
    async fn gas_price(&self) -> Result<u128, RpcError> {
        self.fake.gas_price().await
    }
}

#[tokio::test(start_paused = true)]
async fn twenty_slow_reconciliations_finish_in_three_waves_of_eight() {
    let repo = InMemoryChannelRepository::with_clock(now());
    for id in 1..=20 {
        open(&repo, [id; 32]).await;
        repo.mark_closing(
            &[id; 32],
            PendingClose {
                tx_hash: [id; 32],
                cumulative: 100,
                capture: 80,
                valid_before: u64::try_from(now().timestamp()).unwrap(),
            },
            now(),
        )
        .await
        .unwrap();
    }
    let rpc = SlowRpc {
        fake: FakeTempo::default(),
        active: AtomicUsize::new(0),
        maximum: AtomicUsize::new(0),
    };
    let started = tokio::time::Instant::now();
    let result =
        run_supervision_pass(&repo, &rpc, &signer(), chain(), &Default::default(), now()).await;
    assert_eq!(result.reconciliation_pending, 20);
    assert_eq!(rpc.maximum.load(Ordering::SeqCst), 8);
    assert!(started.elapsed() < std::time::Duration::from_millis(400));
}

#[tokio::test(start_paused = true)]
async fn twenty_slow_supervised_closes_finish_in_three_waves_of_eight() {
    let repo = InMemoryChannelRepository::with_clock(now());
    for id in 1..=20 {
        open(&repo, [id; 32]).await;
        spend(&repo, &[id; 32], 80).await;
    }
    let rpc = SlowRpc {
        fake: FakeTempo::default(),
        active: AtomicUsize::new(0),
        maximum: AtomicUsize::new(0),
    };
    answer(&rpc.fake, requested());
    let started = tokio::time::Instant::now();
    let result =
        run_supervision_pass(&repo, &rpc, &signer(), chain(), &Default::default(), now()).await;
    assert_eq!((result.closes_submitted, result.closes_failed), (20, 0));
    assert_eq!(rpc.maximum.load(Ordering::SeqCst), 8);
    assert!(started.elapsed() < std::time::Duration::from_millis(400));
}

#[tokio::test(start_paused = true)]
async fn twenty_slow_settlements_finish_in_three_waves_of_eight() {
    let repo = InMemoryChannelRepository::with_clock(now());
    for id in 1..=20 {
        open(&repo, [id; 32]).await;
        spend(&repo, &[id; 32], 80).await;
        repo.record_chain_state(
            &[id; 32],
            ChannelState {
                close_requested_at: 0,
                ..requested()
            },
            now(),
        )
        .await
        .unwrap();
    }
    let rpc = SlowRpc {
        fake: FakeTempo::default(),
        active: AtomicUsize::new(0),
        maximum: AtomicUsize::new(0),
    };
    answer(
        &rpc.fake,
        ChannelState {
            close_requested_at: 0,
            ..requested()
        },
    );
    let started = tokio::time::Instant::now();
    let result = crate::mpp::session_settlement::run_settlement_pass(
        &repo,
        &rpc,
        &signer(),
        chain(),
        &Default::default(),
        now(),
    )
    .await;
    assert_eq!((result.settled, result.failed), (20, 0));
    assert_eq!(
        rpc.fake.eth_calls(),
        0,
        "fresh lease records avoid duplicate chain reads"
    );
    assert_eq!(rpc.maximum.load(Ordering::SeqCst), 8);
    assert!(started.elapsed() < std::time::Duration::from_millis(400));
}

#[tokio::test]
async fn deadline_captures_stalled_reservations_as_consumed() {
    let (repo, _) = deadline_close_with_live_reservation().await;
    assert_eq!(
        repo.get(&[1; 32]).await.unwrap().unwrap().settled_on_chain,
        170
    );
}

async fn deadline_close_with_live_reservation() -> (Arc<InMemoryChannelRepository>, Uuid) {
    let repo = Arc::new(InMemoryChannelRepository::with_clock(now()));
    open(&repo, [1; 32]).await;
    spend(&repo, &[1; 32], 80).await;
    let request = reserve(&repo, &[1; 32], 90).await;
    let rpc = OrderedRpc {
        fake: FakeTempo::default(),
        repo: repo.clone(),
        id: [1; 32],
        observed_closing: AtomicUsize::new(0),
        cumulative: 200,
        capture: 170,
        charge_during_build: None,
        settled_during_build: None,
    };
    answer(&rpc.fake, requested());
    let result = run_supervision_pass(
        repo.as_ref(),
        &rpc,
        &signer(),
        chain(),
        &Default::default(),
        now() + chrono::Duration::seconds(590),
    )
    .await;
    assert_eq!((result.closes_submitted, result.closes_failed), (1, 0));
    (repo, request)
}

async fn assert_deadline_capture_remains_spent(repo: &InMemoryChannelRepository) {
    let row = repo.get(&[1; 32]).await.unwrap().unwrap();
    assert_eq!(row.state, ChannelLifecycle::Finalized);
    assert_eq!(
        (row.spent, row.reserved, row.settled_on_chain),
        (170, 0, 170)
    );
    assert_eq!(repo.reservation_count(), 0);
}

#[tokio::test]
async fn late_usage_after_deadline_close_preserves_consumed_capture() {
    let (repo, request) = deadline_close_with_live_reservation().await;
    repo.charge(request, 10, 5).await.unwrap();
    assert_deadline_capture_remains_spent(&repo).await;
    assert_eq!(repo.get(&[1; 32]).await.unwrap().unwrap().units, 6);
}

#[tokio::test]
async fn release_after_deadline_close_preserves_consumed_capture() {
    let (repo, request) = deadline_close_with_live_reservation().await;
    assert!(repo.release(request).await.unwrap());
    assert_deadline_capture_remains_spent(&repo).await;
}

#[tokio::test]
async fn reaper_after_deadline_close_preserves_consumed_capture() {
    let (repo, request) = deadline_close_with_live_reservation().await;
    repo.age_reservation(request, chrono::Duration::minutes(121));
    let summary = run_supervision_pass(
        repo.as_ref(),
        &FakeTempo::default(),
        &signer(),
        chain(),
        &Default::default(),
        now() + chrono::Duration::seconds(600),
    )
    .await;
    assert_eq!(summary.stale_reservations_released, 1);
    assert_deadline_capture_remains_spent(&repo).await;
}

struct FinalizingRpc {
    fake: FakeTempo,
    repo: Arc<InMemoryChannelRepository>,
}

#[async_trait]
impl TempoRpc for FinalizingRpc {
    async fn send_raw_transaction_sync(&self, raw: &[u8]) -> Result<RpcReceipt, RpcError> {
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
        self.repo.finalize(&[1; 32], 60).await.unwrap();
        self.fake.gas_price().await
    }
}

#[tokio::test]
async fn independent_finalization_during_settlement_build_prevents_broadcast() {
    let repo = Arc::new(InMemoryChannelRepository::with_clock(now()));
    open(&repo, [1; 32]).await;
    spend(&repo, &[1; 32], 80).await;
    let rpc = FinalizingRpc {
        fake: FakeTempo::default(),
        repo: repo.clone(),
    };
    answer(
        &rpc.fake,
        ChannelState {
            close_requested_at: 0,
            ..requested()
        },
    );
    let summary = crate::mpp::session_settlement::run_settlement_pass(
        repo.as_ref(),
        &rpc,
        &signer(),
        chain(),
        &Default::default(),
        now(),
    )
    .await;
    assert_eq!((summary.settled, summary.failed), (0, 0));
    assert_eq!(rpc.fake.send_calls(), 0);
    assert_eq!(
        repo.get(&[1; 32]).await.unwrap().unwrap().state,
        ChannelLifecycle::Finalized
    );
}

#[tokio::test]
async fn expired_close_finalizes_zero_deposit_instead_of_reopening() {
    let repo = InMemoryChannelRepository::with_clock(now());
    open(&repo, [1; 32]).await;
    repo.mark_closing(
        &[1; 32],
        PendingClose {
            tx_hash: [9; 32],
            cumulative: 100,
            capture: 80,
            valid_before: 0,
        },
        now(),
    )
    .await
    .unwrap();
    let rpc = FakeTempo::default();
    answer(
        &rpc,
        ChannelState {
            settled: 45,
            deposit: 0,
            close_requested_at: 0,
        },
    );
    let summary =
        run_supervision_pass(&repo, &rpc, &signer(), chain(), &Default::default(), now()).await;
    let row = repo.get(&[1; 32]).await.unwrap().unwrap();
    assert_eq!(
        (summary.reconciled_finalized, summary.reconciled_expired),
        (1, 0)
    );
    assert_eq!(
        (row.state, row.settled_on_chain, row.pending_close),
        (ChannelLifecycle::Finalized, 45, None)
    );
}
