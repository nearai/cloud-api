use crate::common::*;

/// Happy path: valid API key with credits returns 200 and the body carries the
/// authoritative `organization_id` + `workspace_id` so downstream gateways
/// don't have to trust caller-supplied tenant headers.
#[tokio::test]
async fn test_check_api_key_valid() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let org_id = org.id.clone();
    // `get_api_key_for_org` creates a key in the org's first workspace; capture
    // that workspace id so we can assert the response surfaces *that* workspace,
    // not just *some* UUID.
    let expected_workspace_id = list_workspaces(&server, org_id.clone())
        .await
        .first()
        .expect("org should have at least one workspace")
        .id
        .clone();
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/check_api_key")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Valid API key should return 200: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["valid"], true);
    assert_eq!(
        body["organization_id"].as_str().unwrap(),
        org_id,
        "response organization_id must match the org the API key belongs to"
    );
    assert_eq!(
        body["workspace_id"].as_str().unwrap(),
        expected_workspace_id,
        "response workspace_id must match the workspace the API key was created in"
    );
    // `api_key_id` lets trusted gateways (inference-proxy) report usage
    // with a shared service token without forwarding the user's `sk-…`.
    let api_key_id = body["api_key_id"]
        .as_str()
        .expect("response must include api_key_id");
    assert!(
        uuid::Uuid::parse_str(api_key_id).is_ok(),
        "api_key_id must be a UUID, got {api_key_id:?}"
    );
}

/// Invalid API key returns 401.
#[tokio::test]
async fn test_check_api_key_invalid() {
    let server = setup_test_server().await;

    let response = server
        .post("/v1/check_api_key")
        .add_header(
            "Authorization",
            "Bearer sk-00000000000000000000000000000000",
        )
        .await;

    assert_eq!(response.status_code(), 401);
}

/// Missing authorization header returns 401.
#[tokio::test]
async fn test_check_api_key_missing_auth() {
    let server = setup_test_server().await;

    let response = server.post("/v1/check_api_key").await;

    assert_eq!(response.status_code(), 401);
}

/// Organization with no credits returns 402.
#[tokio::test]
async fn test_check_api_key_no_credits() {
    let server = setup_test_server().await;
    // Create org with $0 credits
    let org = setup_org_with_credits(&server, 0i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/check_api_key")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        response.status_code(),
        402,
        "No credits should return 402: {}",
        response.text()
    );
}

/// Regression for nearai/infra#242: a gateway validates the caller's key once
/// per request, so key checks arrive at the caller's full request rate. A
/// burst above the per-key inference limit must keep returning 200, and the
/// checks must not spend the key's inference allowance.
#[tokio::test]
async fn test_check_api_key_burst_above_per_key_rate_limit() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let authorization = format!("Bearer {api_key}");
    let limit = api::middleware::rate_limit::DEFAULT_API_KEY_RATE_LIMIT;

    for attempt in 1..=limit + 1 {
        let response = server
            .post("/v1/check_api_key")
            .add_header("Authorization", authorization.clone())
            .await;
        assert_eq!(
            response.status_code(),
            200,
            "key check {attempt} must not be rate limited (per-key limit {limit}/min): {}",
            response.text()
        );
    }

    // The key checks left the key's inference bucket untouched. An empty body
    // is rejected with 400 by the handler after the limiter has counted the
    // request, so no model is needed. The bucket's 60 s window starts at the
    // first counted inference request, so on a slow run the counter can
    // expire and restart mid-drain: tolerate a reset while still proving the
    // bucket was untouched and still enforces the limit.
    let inference = || {
        server
            .post("/v1/chat/completions")
            .add_header("Authorization", authorization.clone())
            .json(&serde_json::json!({}))
    };
    let response = inference().await;
    assert_eq!(
        response.status_code(),
        400,
        "inference request 1 must be admitted (the key checks did not spend the bucket): {}",
        response.text()
    );
    let mut first_429 = None;
    for attempt in 2..=2 * limit {
        let response = inference().await;
        match response.status_code().as_u16() {
            400 => {}
            429 => {
                let error: api::models::ErrorResponse = response.json();
                assert_eq!(error.error.r#type, "rate_limit_exceeded");
                first_429 = Some(attempt);
                break;
            }
            other => panic!(
                "inference request {attempt}: unexpected status {other}: {}",
                response.text()
            ),
        }
    }
    let first_429 = first_429.unwrap_or_else(|| {
        panic!(
            "no 429 within {} inference requests: the per-key limit is not enforced",
            2 * limit
        )
    });
    assert!(
        first_429 > limit,
        "inference request {first_429} was rate limited before the per-key limit \
         ({limit}/min) was reached: the key checks spent the bucket"
    );

    // An exhausted inference bucket does not block key checks either.
    let response = server
        .post("/v1/check_api_key")
        .add_header("Authorization", authorization)
        .await;
    assert_eq!(
        response.status_code(),
        200,
        "key check after the inference bucket is exhausted: {}",
        response.text()
    );
}
