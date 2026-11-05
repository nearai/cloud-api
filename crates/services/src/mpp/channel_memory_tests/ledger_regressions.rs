use super::*;

#[tokio::test]
async fn retained_vouchers_keep_best_delivered_amount_across_many_large_promotions() {
    let repo = InMemoryChannelRepository::new();
    open(&repo, 1, 100).await;
    let (request, _) = reserved(&repo, 1, 50).await;
    repo.charge(request, 50, 1).await.unwrap();
    repo.accept_voucher(&id(1), 200, sig(2), RULES)
        .await
        .unwrap();
    // The original voucher exceeds delivered, so none is yet eligible.
    // Deliver through the original voucher before advancing beyond the retention cap.
    let (request, _) = reserved(&repo, 1, 50).await;
    repo.charge(request, 50, 1).await.unwrap();
    for amount in 201..240 {
        repo.accept_voucher(&id(1), amount, sig(3), RULES)
            .await
            .unwrap();
    }
    let row = repo.get(&id(1)).await.unwrap().unwrap();
    assert_eq!(row.settleable_voucher().unwrap().cumulative, 100);
    assert_eq!(row.retained_vouchers.len(), MAX_RETAINED_VOUCHERS);
}

#[tokio::test]
async fn chain_read_zero_clears_previous_close_request_and_unchanged_data_preserves_update_time() {
    let repo = InMemoryChannelRepository::new();
    open(&repo, 1, 100).await;
    let at = test_now();
    let state = |close_requested_at| ChannelState {
        deposit: 1_000_000,
        settled: 0,
        close_requested_at,
    };
    repo.record_chain_state(&id(1), state(1), at).await.unwrap();
    let cleared = repo
        .record_chain_state(&id(1), state(0), at + Duration::seconds(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cleared.close_requested_at, 0);
    let writes = repo.chain_state_write_count();
    let same = repo
        .record_chain_state(&id(1), state(0), at + Duration::seconds(2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(same.updated_at, cleared.updated_at);
    assert_eq!(same.last_chain_read_at, Some(at + Duration::seconds(2)));
    assert_eq!(repo.chain_state_write_count(), writes);
}

#[tokio::test]
async fn failed_claim_clear_is_exact_and_expired_claim_never_becomes_successful() {
    let repo = InMemoryChannelRepository::new();
    open(&repo, 1, 100).await;
    let now = test_now();
    let interval = Duration::seconds(60);
    assert!(repo.claim_chain_read(&id(1), now, interval).await.unwrap());
    let old = crate::mpp::ports::chain_read_claim_at(now, interval).unwrap();
    assert!(!crate::mpp::ports::chain_read_is_fresh(
        Some(old),
        now + interval,
        interval
    ));
    assert!(repo
        .claim_chain_read(&id(1), now + interval, interval)
        .await
        .unwrap());
    let replacement = crate::mpp::ports::chain_read_claim_at(now + interval, interval).unwrap();
    assert!(!repo.clear_chain_read_claim(&id(1), old).await.unwrap());
    assert_eq!(
        repo.get(&id(1)).await.unwrap().unwrap().last_chain_read_at,
        Some(replacement)
    );
    assert!(repo
        .clear_chain_read_claim(&id(1), replacement)
        .await
        .unwrap());
    assert!(repo
        .claim_chain_read(&id(1), now + interval, interval)
        .await
        .unwrap());
}
