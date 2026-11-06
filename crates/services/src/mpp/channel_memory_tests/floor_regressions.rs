use super::*;

fn assert_floor(row: &ChannelRecord) {
    assert_eq!((row.spent, row.highest_cumulative), (150, 150));
    assert_eq!(row.highest_signature, sig(100));
    assert_invariants(row);
}

#[tokio::test]
async fn closing_capture_above_voucher_survives_stale_reaper() {
    let repo = InMemoryChannelRepository::new();
    open(&repo, 1, 100).await;
    let (request, _) = reserved(&repo, 1, 40).await;
    repo.record_chain_state(
        &id(1),
        ChannelState {
            settled: 150,
            deposit: 1_000_000,
            close_requested_at: 0,
        },
        test_now(),
    )
    .await
    .unwrap();
    repo.mark_closing(
        &id(1),
        PendingClose {
            capture: 150,
            cumulative: 150,
            ..pending()
        },
        test_now(),
    )
    .await
    .unwrap();
    repo.age_reservation(request, Duration::hours(3));
    assert_eq!(
        repo.release_stale_reservations(repo.now() - Duration::hours(2))
            .await
            .unwrap(),
        1
    );
    let row = repo.get(&id(1)).await.unwrap().unwrap();
    assert_floor(&row);
    assert_eq!(row.reserved, 0);
}

#[tokio::test]
async fn chain_read_above_voucher_then_finalize_zero_raises_floor() {
    let repo = InMemoryChannelRepository::new();
    open(&repo, 1, 100).await;
    let id = id(1);
    let row = repo
        .record_chain_state(
            &id,
            ChannelState {
                settled: 150,
                deposit: 1_000_000,
                close_requested_at: 0,
            },
            test_now(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.settled_on_chain, row.spent, row.highest_cumulative),
        (150, 0, 100)
    );
    repo.finalize(&id, 0).await.unwrap();
    let row = repo.get(&id).await.unwrap().unwrap();
    assert_floor(&row);
    assert_eq!(row.settled_on_chain, 150);
    assert_eq!(row.state, ChannelLifecycle::Finalized);
    assert_eq!(row.settleable_voucher(), None);
}

#[tokio::test]
async fn finalize_argument_above_voucher_raises_floor() {
    let repo = InMemoryChannelRepository::new();
    open(&repo, 1, 100).await;
    let id = id(1);
    repo.finalize(&id, 150).await.unwrap();
    let row = repo.get(&id).await.unwrap().unwrap();
    assert_floor(&row);
    assert_eq!(row.settled_on_chain, 150);
    assert_eq!(row.settleable_voucher(), None);
}

#[tokio::test]
async fn pending_close_finalization_above_voucher_raises_floor() {
    let repo = InMemoryChannelRepository::new();
    open(&repo, 1, 100).await;
    let id = id(1);
    let close = PendingClose {
        capture: 150,
        cumulative: 150,
        ..pending()
    };
    assert!(matches!(
        repo.mark_closing(&id, close, test_now()).await.unwrap(),
        CloseTransition::Marked(_)
    ));
    assert!(repo
        .finalize_pending_close(&id, &close.tx_hash, 150)
        .await
        .unwrap());
    let row = repo.get(&id).await.unwrap().unwrap();
    assert_floor(&row);
    assert_eq!(row.settled_on_chain, 150);
    assert_eq!(row.state, ChannelLifecycle::Finalized);
    assert_eq!(row.settleable_voucher(), None);
}

#[tokio::test]
async fn closing_capture_above_voucher_survives_charge_and_release() {
    for charge in [true, false] {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        let id = id(1);
        let (request, _) = reserved(&repo, 1, 40).await;
        let close = PendingClose {
            capture: 150,
            cumulative: 150,
            ..pending()
        };
        assert!(matches!(
            repo.mark_closing(&id, close, test_now()).await.unwrap(),
            CloseTransition::Marked(_)
        ));
        if charge {
            let (_, paid) = charge_ok(repo.charge(request, 10, 1).await.unwrap());
            assert_eq!(paid, 10);
        } else {
            assert!(repo.release(request).await.unwrap());
        }
        let row = repo.get(&id).await.unwrap().unwrap();
        assert_floor(&row);
        assert_eq!(row.reserved, 0);
        assert_eq!(row.state, ChannelLifecycle::Closing);
        assert_eq!(row.pending_close, Some(close));
        assert_eq!(repo.reservation_count(), 0);
    }
}

#[tokio::test]
async fn refresh_chain_settlement_then_finalize_raises_floor() {
    let repo = InMemoryChannelRepository::new();
    open(&repo, 1, 100).await;
    let id = id(1);
    let mut refresh = new_channel(1, 1_000_000, 100);
    refresh.settled_on_chain = 150;
    let row = repo.open_or_refresh(refresh).await.unwrap();
    assert_eq!(
        (row.settled_on_chain, row.spent, row.highest_cumulative),
        (150, 0, 100)
    );
    repo.finalize(&id, 0).await.unwrap();
    assert_floor(&repo.get(&id).await.unwrap().unwrap());
}

#[tokio::test]
async fn recorded_settlement_then_finalize_raises_floor() {
    let repo = InMemoryChannelRepository::new();
    open(&repo, 1, 100).await;
    let id = id(1);
    assert!(settle(&repo, &id, 150, test_now()).await.unwrap());
    let row = repo.get(&id).await.unwrap().unwrap();
    assert_eq!(
        (row.settled_on_chain, row.spent, row.highest_cumulative),
        (150, 0, 100)
    );
    repo.finalize(&id, 0).await.unwrap();
    assert_floor(&repo.get(&id).await.unwrap().unwrap());
}

#[tokio::test]
async fn overrun_preserves_the_other_reservation() {
    let repo = InMemoryChannelRepository::new();
    open(&repo, 1, 100).await;
    let (first, _) = reserved(&repo, 1, 40).await;
    let (second, _) = reserved(&repo, 1, 40).await;
    let (row, paid) = charge_ok(repo.charge(first, 500, 1).await.unwrap());
    assert_eq!((paid, row.spent, row.reserved), (60, 100, 40));
    assert_invariants(&row);
    let (row, paid) = charge_ok(repo.charge(second, 40, 1).await.unwrap());
    assert_eq!((paid, row.spent, row.reserved), (40, 100, 0));
    assert_invariants(&row);
    assert_eq!(repo.reservation_count(), 0);
}
