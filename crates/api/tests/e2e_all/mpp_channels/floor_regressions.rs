// INTEGRATION: mpp_channels.rs owns the fixture and includes this regression module.
use super::*;

fn assert_floor(row: &ChannelRecord) {
    assert_eq!((row.spent, row.highest_cumulative), (150, 150));
    assert_eq!(row.highest_signature, sig(100));
    assert_invariants(row);
}

#[tokio::test]
async fn chain_read_above_voucher_then_finalize_zero_raises_floor() {
    let f = fixture().await;
    let (id, _) = f.open(100).await;
    let row = f
        .repo
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
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.settled_on_chain, row.spent, row.highest_cumulative),
        (150, 0, 100)
    );
    f.repo.finalize(&id, 0).await.unwrap();
    let row = f.stored(&id).await;
    assert_floor(&row);
    assert_eq!(row.settled_on_chain, 150);
    assert_eq!(row.state, ChannelLifecycle::Finalized);
    assert_eq!(row.settleable_voucher(), None);
}

#[tokio::test]
async fn finalize_argument_above_voucher_raises_floor() {
    let f = fixture().await;
    let (id, _) = f.open(100).await;
    f.repo.finalize(&id, 150).await.unwrap();
    let row = f.stored(&id).await;
    assert_floor(&row);
    assert_eq!(row.settled_on_chain, 150);
    assert_eq!(row.settleable_voucher(), None);
}

#[tokio::test]
async fn pending_close_finalization_above_voucher_raises_floor() {
    let f = fixture().await;
    let (id, _) = f.open(100).await;
    let close = PendingClose {
        capture: 150,
        cumulative: 150,
        ..pending()
    };
    assert!(matches!(
        f.repo.mark_closing(&id, close, test_now()).await.unwrap(),
        CloseTransition::Marked(_)
    ));
    assert!(f
        .repo
        .finalize_pending_close(&id, &close.tx_hash, 150)
        .await
        .unwrap());
    let row = f.stored(&id).await;
    assert_floor(&row);
    assert_eq!(row.settled_on_chain, 150);
    assert_eq!(row.state, ChannelLifecycle::Finalized);
    assert_eq!(row.settleable_voucher(), None);
}

#[tokio::test]
async fn closing_capture_above_voucher_survives_charge_and_release() {
    for charge in [true, false] {
        let f = fixture().await;
        let (id, _) = f.open(100).await;
        let (request, _) = f.reserve(&id, 40).await;
        let close = PendingClose {
            capture: 150,
            cumulative: 150,
            ..pending()
        };
        assert!(matches!(
            f.repo.mark_closing(&id, close, test_now()).await.unwrap(),
            CloseTransition::Marked(_)
        ));
        if charge {
            let (_, paid) = charged(f.repo.charge(request, 10, 1).await.unwrap());
            assert_eq!(paid, 10);
        } else {
            assert!(f.repo.release(request).await.unwrap());
        }
        let row = f.stored(&id).await;
        assert_floor(&row);
        assert_eq!(row.reserved, 0);
        assert_eq!(row.state, ChannelLifecycle::Closing);
        assert_eq!(row.pending_close, Some(close));
        assert_eq!(f.reservations(&id).await, 0);
    }
}

#[tokio::test]
async fn refresh_chain_settlement_then_finalize_raises_floor() {
    let f = fixture().await;
    let (id, _) = f.open(100).await;
    let mut refresh = new_channel(id, 100);
    refresh.settled_on_chain = 150;
    let row = f.repo.open_or_refresh(refresh).await.unwrap();
    assert_eq!(
        (row.settled_on_chain, row.spent, row.highest_cumulative),
        (150, 0, 100)
    );
    f.repo.finalize(&id, 0).await.unwrap();
    assert_floor(&f.stored(&id).await);
}

#[tokio::test]
async fn recorded_settlement_then_finalize_raises_floor() {
    let f = fixture().await;
    let (id, _) = f.open(100).await;
    assert!(f.settle(&id, 150, test_now()).await.unwrap());
    let row = f.stored(&id).await;
    assert_eq!(
        (row.settled_on_chain, row.spent, row.highest_cumulative),
        (150, 0, 100)
    );
    f.repo.finalize(&id, 0).await.unwrap();
    assert_floor(&f.stored(&id).await);
}

#[tokio::test]
async fn overrun_preserves_the_other_reservation() {
    let f = fixture().await;
    let (id, _) = f.open(100).await;
    let (first, _) = f.reserve(&id, 40).await;
    let (second, _) = f.reserve(&id, 40).await;
    let (row, paid) = charged(f.repo.charge(first, 500, 1).await.unwrap());
    assert_eq!((paid, row.spent, row.reserved), (60, 100, 40));
    assert_invariants(&row);
    let (row, paid) = charged(f.repo.charge(second, 40, 1).await.unwrap());
    assert_eq!((paid, row.spent, row.reserved), (40, 100, 0));
    assert_invariants(&row);
    assert_eq!(f.reservations(&id).await, 0);
}
