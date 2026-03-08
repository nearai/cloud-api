//! E2E tests for GET /v1/web/search standalone endpoint.

mod common;

use common::*;

/// When web_search service is not configured, GET /v1/web/search returns 503.
#[tokio::test]
async fn test_web_search_returns_503_when_service_not_configured() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let response = server
        .get("/v1/web/search")
        .add_query_param("q", "test")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        response.status_code(),
        503,
        "When web_search service is not configured, expect 503: {}",
        response.text()
    );
    let body: serde_json::Value = response.json();
    assert_eq!(
        body["error"]["type"].as_str(),
        Some("service_unavailable"),
        "Error type should be service_unavailable"
    );
}
