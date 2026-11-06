use super::*;
use services::mpp::VoucherRules;

#[tokio::test]
async fn closing_capture_survives_smaller_charge_before_finalization() {
    for pending_finalization in [false, true] {
        let f = fixture().await;
        let (id, _) = f.open(300).await;
        let (request, _) = f.reserve(&id, 100).await;
        let close = PendingClose {
            capture: 100,
            cumulative: 300,
            ..pending()
        };
        assert!(matches!(
            f.repo.mark_closing(&id, close, test_now()).await.unwrap(),
            CloseTransition::Marked(_)
        ));
        let charged = f.repo.charge(request, 10, 5).await.unwrap();
        assert!(matches!(
            charged,
            ChargeOutcome::Charged { charged: 10, .. }
        ));
        let row = f.stored(&id).await;
        assert_eq!((row.spent, row.reserved, row.units), (100, 0, 5));
        if pending_finalization {
            assert!(f
                .repo
                .finalize_pending_close(&id, &close.tx_hash, 100)
                .await
                .unwrap());
        } else {
            f.repo.finalize(&id, 100).await.unwrap();
        }
        let row = f.stored(&id).await;
        assert_eq!((row.spent, row.settled_on_chain), (100, 100));
        assert_eq!(row.state, ChannelLifecycle::Finalized);
    }
}

#[tokio::test]
async fn closing_capture_survives_release_and_reaper_before_finalization() {
    for stale in [false, true] {
        let f = fixture().await;
        let (id, _) = f.open(300).await;
        let (request, _) = f.reserve(&id, 100).await;
        let close = PendingClose {
            capture: 100,
            cumulative: 300,
            ..pending()
        };
        f.repo.mark_closing(&id, close, test_now()).await.unwrap();
        if stale {
            f.pool.get().await.unwrap().execute("UPDATE mpp_channel_reservations SET created_at = NOW() - INTERVAL '3 hours' WHERE request_id = $1", &[&request]).await.unwrap();
            f.repo
                .release_stale_reservations(Utc::now() - Duration::hours(2))
                .await
                .unwrap();
        } else {
            assert!(f.repo.release(request).await.unwrap());
        }
        let closing = f.stored(&id).await;
        assert_eq!((closing.spent, closing.reserved), (100, 0));
        assert!(f
            .repo
            .finalize_pending_close(&id, &close.tx_hash, 100)
            .await
            .unwrap());
        let row = f.stored(&id).await;
        assert_eq!(
            (row.spent, row.reserved, row.settled_on_chain),
            (100, 0, 100)
        );
    }
}

#[tokio::test]
async fn finalization_raises_spent_to_the_confirmed_chain_floor() {
    for pending_finalization in [false, true] {
        let f = fixture().await;
        let (id, _) = f.open(300).await;
        let close = PendingClose {
            capture: 0,
            cumulative: 300,
            ..pending()
        };
        if pending_finalization {
            f.repo.mark_closing(&id, close, test_now()).await.unwrap();
            assert!(f
                .repo
                .finalize_pending_close(&id, &close.tx_hash, 120)
                .await
                .unwrap());
        } else {
            f.repo.finalize(&id, 120).await.unwrap();
        }
        let row = f.stored(&id).await;
        assert_eq!((row.spent, row.settled_on_chain), (120, 120));
    }
}

async fn setup(cumulative: u128, deposit: u128) -> (Arc<MppChannelRepository>, [u8; 32]) {
    let f = fixture().await;
    let id = random_id();
    let mut channel = new_channel(id, cumulative);
    channel.deposit = deposit;
    f.repo.open_or_refresh(channel).await.unwrap();
    (f.repo, id)
}

#[tokio::test]
async fn lease_fence_rejects_stale_writes_and_excludes_close() {
    let (repo, id) = setup(100, 1_000).await;
    let now = test_now();
    let until = now + Duration::seconds(30) + Duration::nanoseconds(789);
    let (_, first) = repo
        .claim_settlement_lease(&id, now, until)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.until().timestamp_micros(), until.timestamp_micros());
    assert_eq!(first.until().timestamp_subsec_nanos() % 1_000, 0);
    assert_eq!(
        repo.mark_closing(&id, pending(), now).await.unwrap(),
        CloseTransition::SettlementInFlight
    );
    let later = first.until() + Duration::seconds(1);
    let (_, second) = repo
        .claim_settlement_lease(&id, later, later + Duration::seconds(30))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first, second);
    let before = repo.get(&id).await.unwrap().unwrap();
    assert!(!repo
        .record_settlement(&id, first, 500, later)
        .await
        .unwrap());
    assert_eq!(repo.get(&id).await.unwrap().unwrap(), before);
    assert!(!repo.release_settlement_lease(&id, first).await.unwrap());
    assert_eq!(repo.get(&id).await.unwrap().unwrap(), before);
    assert_eq!(
        repo.mark_closing(&id, pending(), later).await.unwrap(),
        CloseTransition::SettlementInFlight
    );
    assert!(repo
        .record_settlement(&id, second, 75, later)
        .await
        .unwrap());
    let settled = repo.get(&id).await.unwrap().unwrap();
    assert_eq!(settled.settled_on_chain, 75);
    assert_eq!(settled.last_settlement_at, Some(later));
    assert!(!repo
        .record_settlement(&id, second, 90, later)
        .await
        .unwrap());
    assert!(!repo.release_settlement_lease(&id, second).await.unwrap());
    assert!(matches!(
        repo.mark_closing(&id, pending(), later).await.unwrap(),
        CloseTransition::Marked(_)
    ));
    assert!(repo
        .claim_settlement_lease(&id, later, later + Duration::seconds(60))
        .await
        .unwrap()
        .is_none());
    repo.finalize(&id, 80).await.unwrap();
    assert!(repo
        .claim_settlement_lease(&id, later, later + Duration::seconds(60))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_race_has_one_owner_and_only_that_owner_can_release() {
    let (repo, id) = setup(100, 1_000).await;
    let now = test_now();
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut tasks = Vec::new();
    for seconds in [30, 40] {
        let repo = repo.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            repo.claim_settlement_lease(&id, now, now + Duration::seconds(seconds))
                .await
                .unwrap()
        }));
    }
    barrier.wait().await;
    let mut leases = Vec::new();
    for task in tasks {
        if let Some((_, lease)) = task.await.unwrap() {
            leases.push(lease);
        }
    }
    assert_eq!(leases.len(), 1);
    let lease = leases[0];
    assert!(repo.release_settlement_lease(&id, lease).await.unwrap());
    assert!(!repo.release_settlement_lease(&id, lease).await.unwrap());
    assert!(repo
        .claim_settlement_lease(&id, now, now)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn pending_close_hash_fences_clear_and_finalize() {
    let (repo, id) = setup(100, 1_000).await;
    let now = test_now();
    let a = pending();
    assert!(matches!(
        repo.mark_closing(&id, a, now).await.unwrap(),
        CloseTransition::Marked(_)
    ));
    assert!(repo.clear_closing(&id, &a.tx_hash).await.unwrap());
    let b = PendingClose {
        tx_hash: [0x77; 32],
        ..a
    };
    assert!(matches!(
        repo.mark_closing(&id, b, now).await.unwrap(),
        CloseTransition::Marked(_)
    ));
    let before = repo.get(&id).await.unwrap().unwrap();
    assert!(!repo.clear_closing(&id, &a.tx_hash).await.unwrap());
    assert!(!repo
        .finalize_pending_close(&id, &a.tx_hash, 900)
        .await
        .unwrap());
    assert_eq!(repo.get(&id).await.unwrap().unwrap(), before);
    assert!(repo
        .finalize_pending_close(&id, &b.tx_hash, 65)
        .await
        .unwrap());
    let after = repo.get(&id).await.unwrap().unwrap();
    assert_eq!(after.state, ChannelLifecycle::Finalized);
    assert_eq!(after.settled_on_chain, 65);
    assert!(after.pending_close.is_none());
    assert!(!repo.clear_closing(&id, &b.tx_hash).await.unwrap());
    assert!(!repo
        .finalize_pending_close(&id, &b.tx_hash, 900)
        .await
        .unwrap());
    assert_eq!(repo.get(&id).await.unwrap().unwrap(), after);
}

#[tokio::test]
async fn voucher_rules_refuse_each_amount_boundary_without_writing() {
    let (repo, id) = setup(100, 180).await;
    let rules = VoucherRules {
        min_delta: 10,
        settled_floor_from_chain: true,
    };
    for (amount, outcome) in [
        (100, VoucherOutcome::NotHigher),
        (105, VoucherOutcome::DeltaTooSmall),
        (181, VoucherOutcome::AboveDeposit),
    ] {
        let before = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(
            repo.accept_voucher(&id, amount, sig(8), rules)
                .await
                .unwrap(),
            outcome
        );
        assert_eq!(repo.get(&id).await.unwrap().unwrap(), before);
    }
    repo.record_chain_state(
        &id,
        ChannelState {
            deposit: 180,
            settled: 140,
            close_requested_at: 0,
        },
        test_now(),
    )
    .await
    .unwrap();
    let before = repo.get(&id).await.unwrap().unwrap();
    assert_eq!(
        repo.accept_voucher(&id, 130, sig(8), rules).await.unwrap(),
        VoucherOutcome::BelowSettled
    );
    assert_eq!(repo.get(&id).await.unwrap().unwrap(), before);
    // The settled floor is an explicit caller rule; the deposit ceiling always applies.
    assert!(matches!(
        repo.accept_voucher(
            &id,
            130,
            sig(7),
            VoucherRules {
                min_delta: 10,
                settled_floor_from_chain: false
            }
        )
        .await
        .unwrap(),
        VoucherOutcome::Accepted(_)
    ));
    assert!(matches!(
        repo.accept_voucher(&id, 140, sig(9), rules).await.unwrap(),
        VoucherOutcome::Accepted(_)
    ));
    let record = repo.get(&id).await.unwrap().unwrap();
    assert_eq!(
        (record.highest_cumulative, record.highest_signature),
        (140, sig(9))
    );
    assert!(matches!(
        repo.accept_voucher(&id, 180, sig(10), rules).await.unwrap(),
        VoucherOutcome::Accepted(_)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn competing_vouchers_cannot_both_pass_the_minimum_delta() {
    let (repo, id) = setup(100, 1_000).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut tasks = Vec::new();
    for amount in [110, 115] {
        let repo = repo.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            repo.accept_voucher(
                &id,
                amount,
                sig(7),
                VoucherRules {
                    min_delta: 10,
                    settled_floor_from_chain: true,
                },
            )
            .await
            .unwrap()
        }));
    }
    barrier.wait().await;
    let mut accepted = 0;
    for task in tasks {
        match task.await.unwrap() {
            VoucherOutcome::Accepted(_) => accepted += 1,
            VoucherOutcome::NotHigher | VoucherOutcome::DeltaTooSmall => {}
            other => panic!("unexpected outcome {other:?}"),
        }
    }
    assert_eq!(accepted, 1);
    assert!([110, 115].contains(&repo.get(&id).await.unwrap().unwrap().highest_cumulative));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_read_claim_is_once_per_interval_and_old_results_do_not_rewind_it() {
    let (repo, id) = setup(100, 1_000).await;
    let now = test_now();
    let interval = Duration::seconds(60);
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let repo = repo.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            repo.claim_chain_read(&id, now, interval).await.unwrap()
        }));
    }
    barrier.wait().await;
    let mut claims = 0;
    for task in tasks {
        claims += usize::from(task.await.unwrap());
    }
    assert_eq!(claims, 1);
    assert!(!repo
        .claim_chain_read(&id, now + Duration::seconds(59), interval)
        .await
        .unwrap());
    assert!(repo
        .claim_chain_read(&id, now + interval, interval)
        .await
        .unwrap());
    repo.record_chain_state(
        &id,
        ChannelState {
            deposit: 1_000,
            settled: 0,
            close_requested_at: 0,
        },
        now + Duration::seconds(1),
    )
    .await
    .unwrap();
    assert_eq!(
        repo.get(&id).await.unwrap().unwrap().last_chain_read_at,
        services::mpp::ports::chain_read_claim_at(now + interval, interval)
    );
    assert!(!repo
        .claim_chain_read(&id, now + Duration::seconds(61), interval)
        .await
        .unwrap());
    let before = repo.get(&id).await.unwrap().unwrap();
    assert!(!repo.claim_chain_read(&id, now, interval).await.unwrap());
    assert_eq!(repo.get(&id).await.unwrap().unwrap(), before);
}

#[tokio::test]
async fn finalized_capture_survives_late_charge_release_and_reaper() {
    for finalize in [false, true] {
        for case in 0..3 {
            let f = fixture().await;
            let (id, _) = f.open(100).await;
            let (request, _) = f.reserve(&id, 40).await;
            if finalize {
                f.repo.finalize(&id, 150).await.unwrap();
            } else {
                f.repo
                    .record_chain_state(
                        &id,
                        ChannelState {
                            settled: 150,
                            deposit: DEPOSIT,
                            close_requested_at: 0,
                        },
                        test_now(),
                    )
                    .await
                    .unwrap();
                f.repo
                    .mark_closing(
                        &id,
                        PendingClose {
                            capture: 150,
                            cumulative: 150,
                            ..pending()
                        },
                        test_now(),
                    )
                    .await
                    .unwrap();
            }
            match case {
                0 => {
                    f.repo.charge(request, 10, 5).await.unwrap();
                }
                1 => {
                    assert!(f.repo.release(request).await.unwrap());
                }
                _ => {
                    f.pool.get().await.unwrap().execute("UPDATE mpp_channel_reservations SET created_at = NOW() - INTERVAL '3 hours' WHERE request_id = $1", &[&request]).await.unwrap();
                    f.repo
                        .release_stale_reservations(Utc::now() - Duration::hours(2))
                        .await
                        .unwrap();
                }
            }
            let row = f.stored(&id).await;
            assert_eq!(
                (row.spent, row.reserved, row.settled_on_chain),
                (150, 0, 150)
            );
            assert_eq!(row.highest_cumulative, 150);
            assert_invariants(&row);
            assert_eq!(row.units, if case == 0 { 5 } else { 0 });
        }
    }
}
