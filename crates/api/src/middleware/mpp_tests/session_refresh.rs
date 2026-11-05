// INTEGRATION: child of mpp_session_tests.rs; concurrent refresh admission over HTTP.
use super::*;

#[tokio::test]
async fn voucher_during_chain_read_claim_uses_recent_observation_or_returns_503() {
    for (minutes, status) in [(2, StatusCode::OK), (10, StatusCode::SERVICE_UNAVAILABLE)] {
        let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
        seed(&app).await;
        let id = vector_bytes("channelId");
        let now = chrono::Utc::now();
        app.ledger
            .record_chain_state(
                &id,
                ChannelState {
                    deposit: 5_000_000,
                    settled: 0,
                    close_requested_at: 0,
                },
                now - chrono::Duration::minutes(minutes),
            )
            .await
            .unwrap();
        assert!(app
            .ledger
            .claim_chain_read(&id, now, chrono::Duration::seconds(60))
            .await
            .unwrap());
        let response = post_session(
            &app,
            Some(&session_credential(&app, voucher(INITIAL)).await),
        )
        .await;
        assert_eq!(response.status(), status);
        assert_eq!(
            app.state.calls.load(Ordering::SeqCst),
            usize::from(status == StatusCode::OK)
        );
        assert_eq!(app.ledger.reservation_count(), 0);
        if status == StatusCode::OK {
            assert_eq!((row(&app).await.spent, row(&app).await.units), (24, 12));
        } else {
            assert!(!response.headers().contains_key("payment-receipt"));
            assert_eq!(row(&app).await.spent, 0);
        }
    }
}
