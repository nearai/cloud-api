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

/// When web_search service is configured, GET /v1/web/search passes the 503 check.
/// Returns 200 if Brave API works, or 502 if provider fails (e.g. no API key).
#[tokio::test]
async fn test_web_search_passes_503_check_when_service_configured() {
    let server = setup_test_server().await;
    let _service = create_web_search_service(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let response = server
        .get("/v1/web/search")
        .add_query_param("q", "rust programming")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    // Should NOT be 503 (service is configured). May be 200 (Brave works) or 502 (Brave fails).
    assert_ne!(
        response.status_code(),
        503,
        "When web_search service is configured, should not return 503: {}",
        response.text()
    );
}

/// Admin create_service rejects invalid service_name format (e.g. uppercase, hyphen).
#[tokio::test]
async fn test_admin_create_service_rejects_invalid_service_name() {
    let server = setup_test_server().await;

    let invalid_names = ["Web-Search", "web search", "WEB_SEARCH", "invalid.name"];
    for name in invalid_names {
        let request = api::models::CreateServiceRequest {
            service_name: name.to_string(),
            display_name: "Test".to_string(),
            description: None,
            unit: services::service_usage::ports::ServiceUnit::Request,
            cost_per_unit: 1_000_000,
        };
        let response = server
            .post("/v1/admin/services")
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&request)
            .await;

        assert_eq!(
            response.status_code(),
            400,
            "Invalid service_name '{}' should be rejected with 400: {}",
            name,
            response.text()
        );
    }
}
