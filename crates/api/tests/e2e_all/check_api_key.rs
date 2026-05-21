use crate::common::*;

/// Happy path: valid API key with credits returns 200 and the body carries the
/// authoritative `org_id` + `workspace_id` so downstream gateways don't have to
/// trust caller-supplied tenant headers.
#[tokio::test]
async fn test_check_api_key_valid() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let org_id = org.id.clone();
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
        body["org_id"].as_str().unwrap(),
        org_id,
        "response org_id must match the org the API key belongs to"
    );
    let workspace_id = body["workspace_id"]
        .as_str()
        .expect("response must include workspace_id");
    assert!(
        uuid::Uuid::parse_str(workspace_id).is_ok(),
        "workspace_id must be a valid UUID, got {workspace_id}"
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
