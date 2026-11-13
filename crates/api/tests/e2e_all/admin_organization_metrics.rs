// E2E tests for GET /v1/admin/organizations/{org_id}/metrics and the infra summary

use crate::common::*;
use services::admin::{InfraSummary, OrganizationMetrics};

async fn admin_get(server: &axum_test::TestServer, path: &str) -> axum_test::TestResponse {
    server
        .get(path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
}

fn assert_invalid_parameter(response: axum_test::TestResponse) {
    assert_eq!(response.status_code(), 400, "{}", response.text());
    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "invalid_parameter");
}

// ============================================
// Organization Metrics Tests
// ============================================

#[tokio::test]
async fn test_admin_get_organization_metrics_empty() {
    let server = setup_test_server().await;

    // Create an organization (no usage yet)
    let org = create_org(&server).await;

    // Get organization metrics
    let response = server
        .get(format!("/v1/admin/organizations/{}/metrics", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    assert_eq!(
        response.status_code(),
        200,
        "Should successfully get organization metrics"
    );

    let metrics: OrganizationMetrics = serde_json::from_str(&response.text())
        .expect("Failed to parse OrganizationMetrics response");

    println!("Organization metrics: {metrics:#?}");

    // Verify the response structure
    assert_eq!(
        metrics.organization_id.to_string(),
        org.id,
        "Organization ID should match"
    );
    assert!(
        !metrics.organization_name.is_empty(),
        "Should have org name"
    );

    // Verify summary (should be zero/empty for new org)
    assert_eq!(metrics.summary.total_requests, 0, "Should have 0 requests");
    assert_eq!(
        metrics.summary.total_input_tokens, 0,
        "Should have 0 input tokens"
    );
    assert_eq!(
        metrics.summary.total_output_tokens, 0,
        "Should have 0 output tokens"
    );
    assert_eq!(metrics.summary.total_cost_usd, 0.0, "Should have 0 cost");
    assert_eq!(
        metrics.summary.unique_api_keys, 0,
        "Should have 0 unique API keys"
    );

    // An empty org reports zero TTFT counts, not null.
    assert_eq!(
        [
            metrics.summary.ttft_measured_requests,
            metrics.summary.ttft_under_5s_requests,
            metrics.summary.ttft_under_10s_requests,
            metrics.summary.ttft_under_60s_requests,
        ],
        [0, 0, 0, 0]
    );

    // Verify breakdowns - workspaces may have the default workspace with 0 usage
    // API keys and models should be empty since no requests were made
    assert!(metrics.by_api_key.is_empty(), "Should have no API key data");
    assert!(metrics.by_model.is_empty(), "Should have no model data");

    // Check workspace breakdown - should have default workspace with 0 usage
    if !metrics.by_workspace.is_empty() {
        let default_workspace = &metrics.by_workspace[0];
        assert_eq!(
            default_workspace.requests, 0,
            "Default workspace should have 0 requests"
        );
        assert_eq!(
            default_workspace.cost_usd, 0.0,
            "Default workspace should have 0 cost"
        );
    }

    println!("✅ Admin get organization metrics (empty) works correctly");
}

#[tokio::test]
async fn test_admin_get_organization_metrics_with_usage() {
    let server = setup_test_server().await;

    // Create organization with credits
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    // Setup model with pricing
    let model_name = setup_qwen_model(&server).await;

    // Make a few chat completion requests to generate usage
    for i in 0..3 {
        let response = server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&serde_json::json!({
                "model": model_name,
                "messages": [
                    {
                        "role": "user",
                        "content": format!("Say hello {}", i)
                    }
                ],
                "stream": false,
                "max_tokens": 20
            }))
            .await;

        assert_eq!(
            response.status_code(),
            200,
            "Completion request {i} should succeed"
        );
    }

    // Wait for async usage recording to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Get organization metrics
    let response = server
        .get(format!("/v1/admin/organizations/{}/metrics", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    println!("Response status: {}", response.status_code());
    assert_eq!(response.status_code(), 200);

    let metrics: OrganizationMetrics = serde_json::from_str(&response.text())
        .expect("Failed to parse OrganizationMetrics response");

    println!("Organization metrics with usage: {metrics:#?}");

    // Verify summary shows usage
    assert!(
        metrics.summary.total_requests >= 3,
        "Should have at least 3 requests, got {}",
        metrics.summary.total_requests
    );
    assert!(
        metrics.summary.total_input_tokens > 0,
        "Should have input tokens"
    );
    assert!(
        metrics.summary.total_output_tokens > 0,
        "Should have output tokens"
    );
    assert!(metrics.summary.total_cost_usd > 0.0, "Should have cost");
    assert!(
        metrics.summary.unique_api_keys >= 1,
        "Should have at least 1 unique API key"
    );

    // Verify model breakdown
    assert!(!metrics.by_model.is_empty(), "Should have model data");
    let model_metric = metrics
        .by_model
        .iter()
        .find(|m| m.model_name == model_name)
        .expect("Should find the test model in metrics");

    assert!(
        model_metric.requests >= 3,
        "Model should have at least 3 requests"
    );
    assert!(model_metric.cost_usd > 0.0, "Model should have cost");

    // Verify API key breakdown
    assert!(!metrics.by_api_key.is_empty(), "Should have API key data");

    println!("✅ Admin get organization metrics with usage works correctly");
}

#[tokio::test]
async fn test_admin_get_organization_metrics_with_time_range() {
    let server = setup_test_server().await;

    let org = create_org(&server).await;

    // URL-encode `+` because query-form decoding otherwise turns it into a space.
    let now = chrono::Utc::now();
    let week_ago = now - chrono::Duration::days(7);

    let response = server
        .get(
            format!(
                "/v1/admin/organizations/{}/metrics?start={}&end={}",
                org.id,
                week_ago.to_rfc3339().replace('+', "%2B"),
                now.to_rfc3339().replace('+', "%2B")
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    println!("Response status: {}", response.status_code());
    assert_eq!(response.status_code(), 200);

    let metrics: OrganizationMetrics =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    assert!(
        metrics.period_start < metrics.period_end,
        "Period start should be before period end"
    );
    assert_eq!(metrics.period_start, week_ago);
    assert_eq!(metrics.period_end, now);

    assert_eq!(
        metrics.organization_id.to_string(),
        org.id,
        "Organization ID should match"
    );

    println!("✅ Admin get organization metrics with time range works correctly");
}

#[tokio::test]
async fn test_admin_get_organization_metrics_invalid_org() {
    let server = setup_test_server().await;

    // Try to get metrics for non-existent organization
    let fake_org_id = uuid::Uuid::new_v4().to_string();

    let response = server
        .get(format!("/v1/admin/organizations/{fake_org_id}/metrics").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    // Should get an error response (404 or 500) for non-existent org
    // The actual error depends on whether the repo returns NotFound or a general error
    assert!(
        response.status_code() == 404 || response.status_code() == 500,
        "Should get 404 or 500 for non-existent organization, got: {}",
        response.status_code()
    );

    println!("✅ Admin get organization metrics handles invalid org correctly");
}

#[tokio::test]
async fn test_admin_get_organization_metrics_invalid_org_id_format() {
    let server = setup_test_server().await;

    // Try with invalid UUID format
    let response = server
        .get("/v1/admin/organizations/not-a-uuid/metrics")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    println!("Response status: {}", response.status_code());
    assert_eq!(
        response.status_code(),
        400,
        "Should return 400 for invalid UUID format"
    );

    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "invalid_id");

    println!("✅ Admin get organization metrics handles invalid ID format correctly");
}

#[tokio::test]
async fn test_admin_analytics_endpoints_unauthorized() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    // Test organization metrics without auth
    let response = server
        .get(format!("/v1/admin/organizations/{}/metrics", org.id).as_str())
        .await;
    assert_eq!(
        response.status_code(),
        401,
        "Organization metrics should require auth"
    );

    println!("✅ Admin analytics endpoints correctly require authentication");
}

#[tokio::test]
#[ignore = "MockAuthService accepts any valid-looking token, so this test cannot verify API key rejection"]
async fn test_admin_analytics_with_api_key_forbidden() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    // Try to access admin analytics with API key (should fail - needs session token)
    let response = server
        .get(format!("/v1/admin/organizations/{}/metrics", org.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    println!("Response status with API key: {}", response.status_code());

    // API keys are not admin tokens, so this should fail with 401 or 403
    assert!(
        response.status_code() == 401 || response.status_code() == 403,
        "API key should not grant access to admin endpoints, got: {}",
        response.status_code()
    );

    println!("✅ Admin analytics correctly rejects API key authentication");
}

// ============================================
// Metrics Data Validation Tests
// ============================================

#[tokio::test]
async fn test_admin_metrics_model_latency_tracking() {
    let server = setup_test_server().await;

    // Create org and make streaming request (which tracks latency metrics)
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model_name = setup_qwen_model(&server).await;

    // Make a streaming request (latency metrics are tracked for streaming)
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hello streaming!"}],
            "stream": true,
            "max_tokens": 50
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // Wait for usage recording
    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    // Get organization metrics
    let response = server
        .get(format!("/v1/admin/organizations/{}/metrics", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);

    let metrics: OrganizationMetrics =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    println!("Model metrics with latency: {:#?}", metrics.by_model);

    // Check if model metrics include latency fields
    if let Some(model_metric) = metrics.by_model.iter().find(|m| m.model_name == model_name) {
        println!("Model: {}", model_metric.model_name);
        println!("avg_ttft_ms: {:?}", model_metric.avg_ttft_ms);
        println!("p95_ttft_ms: {:?}", model_metric.p95_ttft_ms);
        println!("avg_itl_ms: {:?}", model_metric.avg_itl_ms);
        println!("p95_itl_ms: {:?}", model_metric.p95_itl_ms);

        // Latency fields should be present (may be None for mock provider)
        // Just verify the structure exists
    }

    println!("✅ Admin metrics include model latency tracking fields");
}

#[tokio::test]
async fn test_admin_metrics_unique_api_keys_tracking() {
    let server = setup_test_server().await;

    // Create org with multiple API keys
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    // Create two API keys
    let api_key_1 =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Key 1".to_string()).await;
    let api_key_2 =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Key 2".to_string()).await;

    let model_name = setup_qwen_model(&server).await;

    // Make requests with both API keys
    for api_key in [api_key_1.key.unwrap(), api_key_2.key.unwrap()] {
        let response = server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&serde_json::json!({
                "model": model_name,
                "messages": [{"role": "user", "content": "Hello!"}],
                "stream": false,
                "max_tokens": 20
            }))
            .await;

        assert_eq!(response.status_code(), 200);
    }

    // Wait for usage recording
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Get organization metrics
    let response = server
        .get(format!("/v1/admin/organizations/{}/metrics", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);

    let metrics: OrganizationMetrics =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    println!("Unique API keys: {}", metrics.summary.unique_api_keys);
    println!("API key breakdown: {:#?}", metrics.by_api_key);

    // Should have at least 2 unique API keys
    assert!(
        metrics.summary.unique_api_keys >= 2,
        "Should have at least 2 unique API keys, got {}",
        metrics.summary.unique_api_keys
    );

    // Should have breakdown by API key
    assert!(
        metrics.by_api_key.len() >= 2,
        "Should have breakdown for at least 2 API keys"
    );

    println!("✅ Admin metrics correctly track unique API keys");
}

#[tokio::test]
async fn test_admin_platform_infra_summary_graceful() {
    let server = setup_test_server().await;

    // The machines endpoint is not reachable in tests; the handler must degrade
    // gracefully (200 + stale flag), never 500.
    let response = server
        .get("/v1/admin/platform/infra-summary")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200);
    let infra: InfraSummary = serde_json::from_str(&response.text()).expect("parse InfraSummary");
    assert!(infra.cost_per_host_usd_month >= 0.0);
    // Unconfigured/unreachable inventory in tests -> zero hosts, marked stale.
    assert!(
        infra.stale && infra.total_hosts == 0,
        "unconfigured fleet should be stale with 0 hosts"
    );
    assert!(
        infra.gpu_data_stale
            && infra.total_allocated_gpus == 0
            && infra.model_gpu_allocations.is_empty(),
        "unconfigured Prometheus should be stale with no claimed GPU allocation"
    );

    println!("✅ Platform infra summary degrades gracefully");
}

#[tokio::test]
async fn organization_metrics_rejects_malformed_and_reversed_bounds() {
    let server = setup_test_server().await;
    let organization = create_org(&server).await;

    for query in [
        "start=not-a-timestamp",
        "end=not-a-timestamp",
        "start=2026-09-22T00:00:00Z&end=2026-09-21T00:00:00Z",
        "start=2026-09-22T00:00:00Z&end=2026-09-22T00:00:00Z",
    ] {
        assert_invalid_parameter(
            admin_get(
                &server,
                &format!(
                    "/v1/admin/organizations/{}/metrics?{query}",
                    organization.id
                ),
            )
            .await,
        );
    }
}

#[tokio::test]
async fn organization_metrics_uses_30_day_default_and_366_day_maximum() {
    let server = setup_test_server().await;
    let organization = create_org(&server).await;

    let default = admin_get(
        &server,
        &format!("/v1/admin/organizations/{}/metrics", organization.id),
    )
    .await;
    assert_eq!(default.status_code(), 200, "{}", default.text());
    let metrics = default.json::<OrganizationMetrics>();
    assert_eq!(
        metrics.period_end - metrics.period_start,
        chrono::Duration::days(30)
    );

    let at_max = admin_get(
        &server,
        &format!(
            "/v1/admin/organizations/{}/metrics?start=2025-09-21T00:00:00Z&end=2026-09-22T00:00:00Z",
            organization.id
        ),
    )
    .await;
    assert_eq!(at_max.status_code(), 200, "{}", at_max.text());

    assert_invalid_parameter(
        admin_get(
            &server,
            &format!(
                "/v1/admin/organizations/{}/metrics?start=2025-09-20T23:59:59Z&end=2026-09-22T00:00:00Z",
                organization.id
            ),
        )
        .await,
    );
}
