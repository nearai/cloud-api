//! Charge recovery against PostgreSQL and the production inference router.

use super::*;

#[tokio::test]
async fn reclaim_enforces_two_hour_minimum_even_with_an_aggressive_cutoff() {
    let (_server, database) = setup_test_server_with_database().await;
    let repo = repository(&database);
    let recent = unique_key();
    let abandoned = unique_key();
    claimed(&repo, &recent, "a").await;
    claimed(&repo, &abandoned, "a").await;
    let client = database.pool().get().await.unwrap();
    client.execute("UPDATE mpp_payment_claims SET updated_at = NOW() - INTERVAL '119 minutes' WHERE payment_key = $1", &[&recent]).await.unwrap();
    client.execute("UPDATE mpp_payment_claims SET updated_at = NOW() - INTERVAL '121 minutes' WHERE payment_key = $1", &[&abandoned]).await.unwrap();
    repo.reclaim_stale_serving(chrono::Utc::now())
        .await
        .unwrap();
    assert_eq!(
        state_of(&database, &recent).await.as_deref(),
        Some("serving")
    );
    assert_eq!(
        state_of(&database, &abandoned).await.as_deref(),
        Some("unserved")
    );
}

#[tokio::test]
async fn reclaim_releases_stale_serving_and_preserves_the_new_attempts_lease() {
    let (_server, database) = setup_test_server_with_database().await;
    let repo = repository(&database);
    let stale = unique_key();
    let fresh = unique_key();
    let served = unique_key();
    let unserved = unique_key();
    claimed(&repo, &stale, "a").await;
    claimed(&repo, &fresh, "a").await;
    let done = claimed(&repo, &served, "a").await;
    repo.finish(&served, done, ServeOutcome::Served)
        .await
        .unwrap();
    let failed = claimed(&repo, &unserved, "a").await;
    repo.finish(&unserved, failed, ServeOutcome::Unserved)
        .await
        .unwrap();
    let mut client = database.pool().get().await.unwrap();
    client.execute(
        "UPDATE mpp_payment_claims SET updated_at = NOW() - INTERVAL '121 minutes' WHERE payment_key = ANY($1)",
        &[&vec![stale.clone(), served.clone(), unserved.clone()]],
    ).await.unwrap();
    let abandoned = ClaimLease::new(
        client
            .query_one(
                "SELECT updated_at FROM mpp_payment_claims WHERE payment_key = $1",
                &[&stale],
            )
            .await
            .unwrap()
            .get(0),
    );
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(2);
    // A locked stale row must not block maintenance on another connection.
    let transaction = client.transaction().await.unwrap();
    transaction
        .query_one(
            "SELECT payment_key FROM mpp_payment_claims WHERE payment_key = $1 FOR UPDATE",
            &[&stale],
        )
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        repo.reclaim_stale_serving(cutoff),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        state_of(&database, &stale).await.as_deref(),
        Some("serving")
    );
    transaction.rollback().await.unwrap();
    repo.reclaim_stale_serving(cutoff).await.unwrap();
    assert_eq!(
        state_of(&database, &stale).await.as_deref(),
        Some("unserved")
    );
    assert_eq!(
        state_of(&database, &fresh).await.as_deref(),
        Some("serving")
    );
    assert_eq!(
        state_of(&database, &served).await.as_deref(),
        Some("served")
    );
    assert_eq!(
        state_of(&database, &unserved).await.as_deref(),
        Some("unserved")
    );
    assert!(!repo.is_spent(&stale, "a").await.unwrap());
    assert!(repo.is_spent(&stale, "b").await.unwrap());
    let retry = claimed(&repo, &stale, "a").await;
    assert_ne!(retry, abandoned);
    repo.finish(&stale, abandoned, ServeOutcome::Served)
        .await
        .unwrap();
    assert_eq!(
        state_of(&database, &stale).await.as_deref(),
        Some("serving")
    );
    repo.finish(&stale, retry, ServeOutcome::Served)
        .await
        .unwrap();
    assert_eq!(state_of(&database, &stale).await.as_deref(), Some("served"));
}

#[tokio::test]
async fn failed_paid_inference_is_unserved_and_the_identical_credential_can_succeed() {
    let bootstrap = setup_test_server().await;
    let org = setup_org_with_credits(&bootstrap, 10_000_000_000i64).await;
    let service_key = get_api_key_for_org(&bootstrap, org.id.clone()).await;
    let tempo_node = MockServer::start().await;
    let settings = mpp_settings(&tempo_node.uri(), &service_key);
    let (server, _pool, mock, database) = setup_test_server_with_pool_and_config(|config| {
        config.mpp = MppConfig::Enabled(Box::new(settings));
    })
    .await;
    let body = json!({
        "model": E2E_QWEN_MODEL_NAME, "max_tokens": 64,
        "messages": [{"role": "user", "content": "MPP recovery regression"}],
    })
    .to_string();
    let post = |authorization: Option<String>| {
        let request = server
            .post("/v1/chat/completions")
            .add_header("Content-Type", "application/json")
            .bytes(body.clone().into());
        match authorization {
            Some(value) => request.add_header("Authorization", value),
            None => request,
        }
    };
    let unpaid = post(None).await;
    assert_eq!(unpaid.status_code(), 402);
    let challenge = parse_challenge(unpaid.header("www-authenticate").to_str().unwrap());
    let mut tx_hash = [0x6b; 32];
    tx_hash[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    let memo = tempo::attribution_memo(REALM, &challenge.id);
    let receipt = payment_receipt(tx_hash, PAYER, CURRENCY, RECIPIENT, AMOUNT, Some(memo));
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "eth_getTransactionReceipt"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": receipt_json(&receipt),
        })))
        .mount(&tempo_node)
        .await;
    let credential = credential_header(&challenge, hash_payload(tx_hash), None);
    let key = protocol::payment_key(SECRET.as_bytes(), &tx_hash).unwrap();
    // A non-retryable provider rejection exercises a real handler failure after claim.
    mock.set_error_override(Some(inference_providers::CompletionError::HttpError {
        status_code: 400,
        message: "synthetic provider rejection".to_string(),
        is_external: false,
    }))
    .await;
    let failed = post(Some(credential.clone())).await;
    assert!(!failed.status_code().is_success(), "{}", failed.text());
    assert!(failed.maybe_header("payment-receipt").is_none());
    assert_eq!(state_of(&database, &key).await.as_deref(), Some("unserved"));
    mock.set_error_override(None).await;
    let retry = post(Some(credential.clone())).await;
    assert_eq!(retry.status_code(), 200, "{}", retry.text());
    assert!(retry.maybe_header("payment-receipt").is_some());
    assert_eq!(state_of(&database, &key).await.as_deref(), Some("served"));
    let replay = post(Some(credential)).await;
    assert_eq!(replay.status_code(), 403, "{}", replay.text());
    assert!(replay.maybe_header("payment-receipt").is_none());
}
