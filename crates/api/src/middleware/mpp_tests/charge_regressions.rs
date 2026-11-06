//! Atomic charge consumption at the innermost settlement boundary.

use super::*;

#[tokio::test]
async fn charge_only_unpriced_payment_inputs_retain_baseline_401_authentication() {
    let app = app(true);
    let charge = app.pay(&body(), TX).await;
    let without_hash = Router::new()
        .route("/v1/chat/completions", post(|| async { StatusCode::OK }))
        .layer(from_fn_with_state(
            MppAuthState {
                auth: auth_state(),
                mpp: app.service.clone(),
                session: None,
            },
            mpp_auth_middleware,
        ));
    let baseline = baseline_router();
    let cases = [
        ("missing body hash", body(), true),
        (
            "unpriced model",
            br#"{"model":"unpriced","max_tokens":8}"#.to_vec(),
            false,
        ),
        ("unparseable body", b"not json".to_vec(), false),
        ("body beyond every price cap", vec![b' '; 65_537], false),
    ];
    for authorization in ["Payment e30", charge.as_str()] {
        for (case, payload, missing_hash) in &cases {
            let router = if *missing_hash {
                &without_hash
            } else {
                &app.router
            };
            let result = send(
                router,
                request("/v1/chat/completions", payload, Some(authorization)),
            )
            .await;
            let original = send(
                &baseline,
                request("/v1/chat/completions", payload, Some(authorization)),
            )
            .await;
            assert_eq!(
                result.status,
                StatusCode::UNAUTHORIZED,
                "{case}: {}",
                result.text()
            );
            assert_eq!(result.status, original.status, "{case}");
            assert_eq!(result.body, original.body, "{case}");
            assert!(result.header("payment-receipt").is_none(), "{case}");
            assert!(result.header("www-authenticate").is_none(), "{case}");
        }
    }
    assert_eq!(app.calls.load(Ordering::SeqCst), 0);
    assert!(app.repo.is_empty());
}

#[tokio::test]
async fn an_already_used_payment_is_403_without_receipt_and_runs_one_handler() {
    let app = app(true);
    let credential = app.pay(&body(), TX).await;
    let service = app.service.as_ref().unwrap().clone();
    let verified = service
        .verify(&credential, CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    let paid = service.verify_payment(&verified).await.unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = app.calls.clone();
    let router = Router::new()
        .route(
            "/",
            post({
                let entered = entered.clone();
                let release = release.clone();
                move || {
                    let calls = calls.clone();
                    let entered = entered.clone();
                    let release = release.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        entered.notify_one();
                        release.notified().await;
                        StatusCode::OK
                    }
                }
            }),
        )
        .layer(from_fn_with_state(
            MppSettleState { mpp: Some(service) },
            mpp_settle_middleware,
        ))
        .layer(Extension(PendingMppPayment(paid)));
    // Both requests have passed verification before either claims. Hold the winning
    // handler open so the loser exercises the atomic claim, not the earlier spent check.
    let first_router = router.clone();
    let first = tokio::spawn(async move { send(&first_router, request("/", &body(), None)).await });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let second = tokio::time::timeout(
        Duration::from_secs(5),
        send(&router, request("/", &body(), None)),
    )
    .await
    .unwrap();
    assert_eq!(second.status, StatusCode::FORBIDDEN);
    assert!(second.header("payment-receipt").is_none());
    assert!(second.header("www-authenticate").is_none());
    assert_eq!(app.calls.load(Ordering::SeqCst), 1);
    release.notify_one();
    let first = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.status, StatusCode::OK);
    assert!(first.header("payment-receipt").is_some());
    assert_eq!(app.calls.load(Ordering::SeqCst), 1);
}
