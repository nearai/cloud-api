mod common;

use common::*;

/// Happy path: valid API key with credits returns 200.
#[tokio::test]
async fn test_check_api_key_valid() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
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
}

/// Invalid API key returns 401.
#[tokio::test]
async fn test_check_api_key_invalid() {
    let (server, _guard) = setup_test_server().await;

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
    let (server, _guard) = setup_test_server().await;

    let response = server.post("/v1/check_api_key").await;

    assert_eq!(response.status_code(), 401);
}

/// Organization with no credits returns 402.
#[tokio::test]
async fn test_check_api_key_no_credits() {
    let (server, _guard) = setup_test_server().await;
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
