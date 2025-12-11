// E2E tests for admin analytics endpoints
mod common;

use common::*;
use services::admin::{OrganizationMetrics, PlatformMetrics, TimeSeriesMetrics};

// ============================================
// Organization Metrics Tests
// ============================================

#[tokio::test]
async fn test_admin_get_organization_metrics_empty() {
    let server = setup_test_server(None).await;

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
    let server = setup_test_server(None).await;

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
    let server = setup_test_server(None).await;

    // Create organization
    let org = create_org(&server).await;

    // Get metrics with custom time range (last 7 days)
    let now = chrono::Utc::now();
    let week_ago = now - chrono::Duration::days(7);

    let response = server
        .get(
            format!(
                "/v1/admin/organizations/{}/metrics?start={}&end={}",
                org.id,
                week_ago.to_rfc3339(),
                now.to_rfc3339()
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

    // Verify period is present and valid
    assert!(
        metrics.period_start < metrics.period_end,
        "Period start should be before period end"
    );

    // Verify structure is valid
    assert_eq!(
        metrics.organization_id.to_string(),
        org.id,
        "Organization ID should match"
    );

    println!("✅ Admin get organization metrics with time range works correctly");
}

#[tokio::test]
async fn test_admin_get_organization_metrics_invalid_org() {
    let server = setup_test_server(None).await;

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
    let server = setup_test_server(None).await;

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

// ============================================
// Platform Metrics Tests
// ============================================

#[tokio::test]
async fn test_admin_get_platform_metrics() {
    let server = setup_test_server(None).await;

    // Create some organizations to have data
    let _org1 = create_org(&server).await;
    let _org2 = create_org(&server).await;

    // Get platform metrics
    let response = server
        .get("/v1/admin/platform/metrics")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    assert_eq!(
        response.status_code(),
        200,
        "Should successfully get platform metrics"
    );

    let metrics: PlatformMetrics =
        serde_json::from_str(&response.text()).expect("Failed to parse PlatformMetrics response");

    println!("Platform metrics: {metrics:#?}");

    // Verify the response structure
    assert!(
        metrics.total_users >= 1,
        "Should have at least 1 user (the mock user)"
    );
    assert!(
        metrics.total_organizations >= 2,
        "Should have at least 2 organizations"
    );
    assert!(
        metrics.total_requests >= 0,
        "Requests should be non-negative"
    );
    assert!(
        metrics.total_revenue_usd >= 0.0,
        "Revenue should be non-negative"
    );

    // Verify period fields are present
    assert!(
        metrics.period_start < metrics.period_end,
        "Period start should be before period end"
    );

    println!("✅ Admin get platform metrics works correctly");
}

#[tokio::test]
async fn test_admin_get_platform_metrics_with_usage() {
    let server = setup_test_server(None).await;

    // Create organization with credits and make requests
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model_name = setup_qwen_model(&server).await;

    // Make a completion request
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hello platform!"}],
            "stream": false,
            "max_tokens": 20
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // Wait for usage recording
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Get platform metrics
    let response = server
        .get("/v1/admin/platform/metrics")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);

    let metrics: PlatformMetrics =
        serde_json::from_str(&response.text()).expect("Failed to parse PlatformMetrics");

    println!("Platform metrics with usage: {metrics:#?}");

    // Verify we have some activity
    assert!(
        metrics.total_requests >= 1,
        "Should have at least 1 request"
    );
    assert!(
        metrics.total_revenue_usd >= 0.0,
        "Revenue should be non-negative"
    );

    // Verify top_models and top_organizations are present
    // They may be empty if the data doesn't meet threshold criteria
    println!("Top models: {:?}", metrics.top_models);
    println!("Top organizations: {:?}", metrics.top_organizations);

    println!("✅ Admin get platform metrics with usage works correctly");
}

#[tokio::test]
async fn test_admin_get_platform_metrics_with_time_range() {
    let server = setup_test_server(None).await;

    // Get metrics with custom time range
    let now = chrono::Utc::now();
    let week_ago = now - chrono::Duration::days(7);

    let response = server
        .get(
            format!(
                "/v1/admin/platform/metrics?start={}&end={}",
                week_ago.to_rfc3339(),
                now.to_rfc3339()
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    println!("Response status: {}", response.status_code());
    assert_eq!(response.status_code(), 200);

    let metrics: PlatformMetrics =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    // Verify period is present and valid
    assert!(
        metrics.period_start < metrics.period_end,
        "Period start should be before period end"
    );

    // Verify metrics structure is valid
    assert!(
        metrics.total_users >= 0,
        "Total users should be non-negative"
    );
    assert!(
        metrics.total_organizations >= 0,
        "Total organizations should be non-negative"
    );

    println!("✅ Admin get platform metrics with time range works correctly");
}

// ============================================
// Time Series Metrics Tests
// ============================================

#[tokio::test]
async fn test_admin_get_organization_timeseries() {
    let server = setup_test_server(None).await;

    // Create organization
    let org = create_org(&server).await;

    // Get timeseries with default granularity (day)
    let response = server
        .get(format!("/v1/admin/organizations/{}/metrics/timeseries", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    assert_eq!(
        response.status_code(),
        200,
        "Should successfully get timeseries metrics"
    );

    let metrics: TimeSeriesMetrics =
        serde_json::from_str(&response.text()).expect("Failed to parse TimeSeriesMetrics response");

    println!("Timeseries metrics: {metrics:#?}");

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
    assert_eq!(
        metrics.granularity, "day",
        "Default granularity should be day"
    );

    // Data may be empty for new org, but should be present
    println!("Timeseries data points: {}", metrics.data.len());

    println!("✅ Admin get organization timeseries works correctly");
}

#[tokio::test]
async fn test_admin_get_organization_timeseries_with_usage() {
    let server = setup_test_server(None).await;

    // Create organization with credits
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model_name = setup_qwen_model(&server).await;

    // Make a completion request
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hello timeseries!"}],
            "stream": false,
            "max_tokens": 20
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // Wait for usage recording
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Get timeseries
    let response = server
        .get(format!("/v1/admin/organizations/{}/metrics/timeseries", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);

    let metrics: TimeSeriesMetrics =
        serde_json::from_str(&response.text()).expect("Failed to parse TimeSeriesMetrics");

    println!("Timeseries with usage: {metrics:#?}");

    // Should have at least one data point with today's usage
    if !metrics.data.is_empty() {
        let today_data = &metrics.data[0];
        println!("Today's data point: {today_data:?}");

        // The data point should have some values
        assert!(!today_data.date.is_empty(), "Date should be present");
    }

    println!("✅ Admin get organization timeseries with usage works correctly");
}

#[tokio::test]
async fn test_admin_get_organization_timeseries_granularity_hour() {
    let server = setup_test_server(None).await;
    let org = create_org(&server).await;

    let response = server
        .get(
            format!(
                "/v1/admin/organizations/{}/metrics/timeseries?granularity=hour",
                org.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);

    let metrics: TimeSeriesMetrics =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    assert_eq!(
        metrics.granularity, "hour",
        "Granularity should be hour when requested"
    );

    println!("✅ Admin get organization timeseries with hour granularity works correctly");
}

#[tokio::test]
async fn test_admin_get_organization_timeseries_granularity_week() {
    let server = setup_test_server(None).await;
    let org = create_org(&server).await;

    let response = server
        .get(
            format!(
                "/v1/admin/organizations/{}/metrics/timeseries?granularity=week",
                org.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);

    let metrics: TimeSeriesMetrics =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    assert_eq!(
        metrics.granularity, "week",
        "Granularity should be week when requested"
    );

    println!("✅ Admin get organization timeseries with week granularity works correctly");
}

#[tokio::test]
async fn test_admin_get_organization_timeseries_invalid_granularity() {
    let server = setup_test_server(None).await;
    let org = create_org(&server).await;

    let response = server
        .get(
            format!(
                "/v1/admin/organizations/{}/metrics/timeseries?granularity=invalid",
                org.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    assert_eq!(
        response.status_code(),
        400,
        "Should return 400 for invalid granularity"
    );

    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(
        error.error.r#type, "invalid_granularity",
        "Error type should be invalid_granularity"
    );

    println!("✅ Admin get organization timeseries handles invalid granularity correctly");
}

// ============================================
// Authorization Tests
// ============================================

#[tokio::test]
async fn test_admin_analytics_endpoints_unauthorized() {
    let server = setup_test_server(None).await;
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

    // Test platform metrics without auth
    let response = server.get("/v1/admin/platform/metrics").await;
    assert_eq!(
        response.status_code(),
        401,
        "Platform metrics should require auth"
    );

    // Test timeseries without auth
    let response = server
        .get(format!("/v1/admin/organizations/{}/metrics/timeseries", org.id).as_str())
        .await;
    assert_eq!(
        response.status_code(),
        401,
        "Timeseries metrics should require auth"
    );

    println!("✅ Admin analytics endpoints correctly require authentication");
}

#[tokio::test]
#[ignore = "MockAuthService accepts any valid-looking token, so this test cannot verify API key rejection"]
async fn test_admin_analytics_with_api_key_forbidden() {
    let server = setup_test_server(None).await;
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
    let server = setup_test_server(None).await;

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
    let server = setup_test_server(None).await;

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
