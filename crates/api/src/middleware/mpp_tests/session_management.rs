//! Management requests use the official client's header-only POST shape.

use crate::middleware::mpp::tests::session::*;

async fn bodyless(app: &SessionApp, authorization: &str) -> Response {
    app.router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header(header::AUTHORIZATION, authorization)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

fn no_inference(app: &SessionApp) {
    assert_eq!(app.auth_calls.load(Ordering::SeqCst), 0);
    assert_eq!(app.state.calls.load(Ordering::SeqCst), 0);
    assert_eq!(app.ledger.reservation_count(), 0);
}

#[tokio::test]
async fn bodyless_close_returns_receipt_without_auth_or_handler() {
    let app = session_app("sk-live-rejected", AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    app.blocked.store(true, Ordering::SeqCst);
    let response = bodyless(&app, &session_credential(&app, close_payload()).await).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(receipt_value(&response)["txHash"].as_str().is_some());
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value, json!({"status":"ok"}));
    assert_eq!(
        row(&app).await.state,
        services::mpp::ChannelLifecycle::Finalized
    );
    no_inference(&app);
}

#[tokio::test]
async fn bodyless_open_and_top_up_return_receipts_without_auth_or_handler() {
    let app = session_app("sk-live-rejected", AMOUNT, Some(SESSION_PRICE));
    set_chain(&app, 5_000_000);
    let raw = hex::decode(vector_text("open.serialized").trim_start_matches("0x")).unwrap();
    app.rpc.mine(RpcReceipt {
        tx_hash: tx_hash(&raw),
        block_number: 1,
        success: true,
        from: vector_bytes("payer"),
        logs: vec![],
    });
    let credential = session_credential(&app, open_payload()).await;
    for _ in 0..2 {
        let response = bodyless(&app, &credential).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            receipt_value(&response)["acceptedCumulative"],
            INITIAL.to_string()
        );
        no_inference(&app);
    }
    let (payload, raw) = top_up_payload();
    app.rpc.mine(RpcReceipt {
        tx_hash: tx_hash(&raw),
        block_number: 2,
        success: true,
        from: vector_bytes("payer"),
        logs: vec![],
    });
    set_chain(&app, 5_000_200);
    let response = bodyless(&app, &session_credential(&app, payload).await).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(receipt_value(&response)["txHash"].as_str().is_some());
    assert_eq!(row(&app).await.deposit, 5_000_200);
    no_inference(&app);
}

#[tokio::test]
async fn bodyless_voucher_and_garbage_return_mpp_problems() {
    let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
    for authorization in [
        session_credential(&app, voucher(INITIAL)).await,
        "Payment garbage".into(),
    ] {
        let response = bodyless(&app, &authorization).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/problem+json"
        );
        let value: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            value["type"],
            "https://paymentauth.org/problems/malformed-credential"
        );
        no_inference(&app);
    }
}

#[tokio::test]
async fn bodyless_bearer_preserves_plain_auth_behavior() {
    let enabled = app(true);
    let disabled = app(false);
    for authorization in ["Bearer sk-wrong", "Bearer sk-live-service-key-0001"] {
        let current = enabled.post(b"", Some(authorization)).await;
        let previous = disabled.post(b"", Some(authorization)).await;
        assert_eq!(current.status, previous.status);
        assert_eq!(current.json(), previous.json());
    }
}

#[tokio::test]
async fn close_below_highest_voucher_captures_spend_and_echoes_signed_amount() {
    // mppx can close at confirmed spend or the accepted amount in an earlier snapshot.
    for amount in [24, 130] {
        let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
        seed(&app).await;
        let response = post_session(
            &app,
            Some(&session_credential(&app, voucher(INITIAL)).await),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(row(&app).await.spent, 24);
        let mut payload = voucher(amount);
        payload["action"] = json!("close");
        let digest = close_authorization_digest(
            &vector_bytes("escrow"),
            42431,
            &vector_bytes("channelId"),
            amount,
        )
        .unwrap();
        let key = k256::SecretKey::from_slice(&vector_bytes::<32>("payerKey")).unwrap();
        payload["closeSignature"] = json!(hex_prefixed(&sign_digest(&key, &digest).unwrap()));
        let response = bodyless(&app, &session_credential(&app, payload).await).await;
        assert_eq!(response.status(), StatusCode::OK);
        let receipt = receipt_value(&response);
        assert_eq!(receipt["acceptedCumulative"], amount.to_string());
        assert_eq!(receipt["spent"], "24");
        assert!(receipt["txHash"].as_str().is_some());
        let row = row(&app).await;
        assert_eq!(row.state, services::mpp::ChannelLifecycle::Finalized);
        assert_eq!(row.highest_cumulative, INITIAL);
        assert_eq!(row.settled_on_chain, 24);
        assert_eq!(app.auth_calls.load(Ordering::SeqCst), 1);
        assert_eq!(app.state.calls.load(Ordering::SeqCst), 1);
    }
}
