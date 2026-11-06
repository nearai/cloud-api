use super::*;

// ---- settlement ------------------------------------------------------------------

#[tokio::test]
async fn the_settlement_lease_is_exclusive_until_it_lapses() {
    let repo = InMemoryChannelRepository::new();
    let now = test_now();
    assert!(repo
        .claim_settlement_lease(&id(1), now, now + Duration::seconds(30))
        .await
        .unwrap()
        .is_none());
    open(&repo, 1, 100).await;
    let until = now + Duration::seconds(30);
    assert!(repo
        .claim_settlement_lease(&id(1), now, until)
        .await
        .unwrap()
        .is_some());
    assert_eq!(repo.lease_until(&id(1)), Some(until));
    // Held: refused while now is before, and exactly at, the expiry.
    for at in [now, now + Duration::seconds(29), until] {
        assert!(repo
            .claim_settlement_lease(&id(1), at, at + Duration::seconds(30))
            .await
            .unwrap()
            .is_none());
    }
    assert_eq!(repo.lease_until(&id(1)), Some(until));
    // Past the expiry another instance takes over.
    let later = until + Duration::seconds(1);
    assert!(repo
        .claim_settlement_lease(&id(1), later, later + Duration::seconds(30))
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn releasing_the_lease_frees_it() {
    let repo = InMemoryChannelRepository::new();
    let now = test_now();
    open(&repo, 1, 100).await;
    let until = now + Duration::seconds(30);
    repo.claim_settlement_lease(&id(1), now, until)
        .await
        .unwrap();
    repo.release_settlement_lease(&id(1), SettlementLease::new(until))
        .await
        .unwrap();
    assert_eq!(repo.lease_until(&id(1)), None);
    assert!(repo
        .claim_settlement_lease(&id(1), now, until)
        .await
        .unwrap()
        .is_some());
    // Unknown channel: nothing to release.
    repo.release_settlement_lease(&id(9), SettlementLease::new(until))
        .await
        .unwrap();
}

#[tokio::test]
async fn record_settlement_drops_covered_vouchers_and_the_lease() {
    let repo = InMemoryChannelRepository::new();
    open(&repo, 1, 100).await;
    for cumulative in [200, 300, 400] {
        repo.accept_voucher(
            &id(1),
            cumulative,
            sig(cumulative as u8),
            crate::mpp::ports::VoucherRules {
                min_delta: 0,
                settled_floor_from_chain: true,
            },
        )
        .await
        .unwrap();
    }
    let (request, _) = reserved(&repo, 1, 150).await;
    charge_ok(repo.charge(request, 150, 15).await.unwrap());
    let now = test_now();
    repo.claim_settlement_lease(&id(1), now, now + Duration::seconds(30))
        .await
        .unwrap();
    let at = now + Duration::seconds(1);
    settle(&repo, &id(1), 200, at).await.unwrap();

    let record = repo.get(&id(1)).await.unwrap().unwrap();
    assert_eq!(record.settled_on_chain, 200);
    assert_eq!(record.last_settlement_at, Some(at));
    assert_eq!(record.last_settlement_spent, 150);
    assert_eq!(
        record
            .retained_vouchers
            .iter()
            .map(|voucher| voucher.cumulative)
            .collect::<Vec<_>>(),
        vec![300],
        "the vouchers at or below the settled amount are gone"
    );
    assert_eq!(repo.lease_until(&id(1)), None);

    // A lower amount lowers nothing, but still stamps the settlement.
    settle(&repo, &id(1), 50, at + Duration::seconds(1))
        .await
        .unwrap();
    let record = repo.get(&id(1)).await.unwrap().unwrap();
    assert_eq!(record.settled_on_chain, 200);
    assert_eq!(record.last_settlement_at, Some(at + Duration::seconds(1)));
    settle(&repo, &id(9), 1, at).await.unwrap();
}

#[tokio::test]
async fn settleable_voucher_never_exceeds_what_was_delivered() {
    let repo = InMemoryChannelRepository::new();
    open(&repo, 1, 100).await;
    for cumulative in [250, 400, 900] {
        repo.accept_voucher(
            &id(1),
            cumulative,
            sig(cumulative as u8),
            crate::mpp::ports::VoucherRules {
                min_delta: 0,
                settled_floor_from_chain: true,
            },
        )
        .await
        .unwrap();
    }
    // Nothing delivered yet: nothing settleable, whatever the vouchers say.
    let record = repo.get(&id(1)).await.unwrap().unwrap();
    assert_eq!(record.settleable_voucher(), None);

    let (request, record) = reserved(&repo, 1, 500).await;
    assert_eq!(
        record.settleable_voucher(),
        None,
        "reserved is not delivered"
    );
    let (record, _) = charge_ok(repo.charge(request, 300, 30).await.unwrap());
    assert_eq!(record.delivered(), 300);
    let voucher = record.settleable_voucher().expect("a voucher below 300");
    assert_eq!(voucher.cumulative, 250);
    assert!(voucher.cumulative <= record.delivered());

    // In-flight reservations keep the highest voucher out of reach.
    let (_inflight, record) = reserved(&repo, 1, 200).await;
    assert_eq!(record.delivered(), 300);
    assert!(record.settleable_voucher().unwrap().cumulative <= record.delivered());
    assert_invariants(&record);

    // After settling the 250 voucher nothing further fits under 300.
    settle(&repo, &id(1), 250, Utc::now()).await.unwrap();
    let record = repo.get(&id(1)).await.unwrap().unwrap();
    assert_eq!(record.settleable_voucher(), None);
}
