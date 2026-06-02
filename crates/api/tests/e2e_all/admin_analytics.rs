// E2E tests for admin analytics endpoints

use crate::common::*;
use services::admin::{
    BillingSummary, InfraSummary, ModelRevenueReport, OrgRevenueReport, OrganizationMetrics,
    PlatformMetrics, PlatformTimeSeriesMetrics, TimeSeriesMetrics,
};

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

// ============================================
// Platform Metrics Tests
// ============================================

#[tokio::test]
async fn test_admin_get_platform_metrics() {
    let server = setup_test_server().await;

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
        metrics.total_consumed_usd >= 0.0,
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
    let server = setup_test_server().await;

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
        metrics.total_consumed_usd >= 0.0,
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
    let server = setup_test_server().await;

    // Get metrics with custom time range. URL-encode the `+` in the `+00:00`
    // offset (otherwise it decodes to a space and is rejected as a bad timestamp).
    let now = chrono::Utc::now();
    let week_ago = now - chrono::Duration::days(7);

    let response = server
        .get(
            format!(
                "/v1/admin/platform/metrics?start={}&end={}",
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
    let server = setup_test_server().await;

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
    let server = setup_test_server().await;

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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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

// ============================================
// Platform Stats Dashboard Tests (new endpoints)
// ============================================

#[tokio::test]
async fn test_admin_platform_metrics_splits_reconcile() {
    let server = setup_test_server().await;

    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model_name = setup_qwen_model(&server).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hello splits!"}],
            "stream": false,
            "max_tokens": 20
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let response = server
        .get("/v1/admin/platform/metrics")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200);

    let m: PlatformMetrics = serde_json::from_str(&response.text()).expect("parse PlatformMetrics");

    // The verifiable split must reconcile to the total (within fp tolerance).
    let eps = 1e-6;
    assert!(
        (m.verifiable_consumed_usd + m.non_verifiable_consumed_usd - m.total_consumed_usd).abs()
            < eps,
        "verifiable + external must equal total consumed"
    );
    assert_eq!(
        m.verifiable_requests + m.non_verifiable_requests,
        m.total_requests,
        "verifiable + external requests must equal total requests"
    );
    assert!(m.active_organizations >= 1, "should have an active org");
    assert!(
        (0.0..=1.0).contains(&m.provider_error_or_timeout_rate),
        "provider_error_or_timeout_rate in [0,1]"
    );

    println!("✅ Platform metrics verifiable split reconciles to total");
}

#[tokio::test]
async fn test_admin_platform_timeseries() {
    let server = setup_test_server().await;

    let response = server
        .get("/v1/admin/platform/metrics/timeseries?granularity=month")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200);
    let ts: PlatformTimeSeriesMetrics =
        serde_json::from_str(&response.text()).expect("parse PlatformTimeSeriesMetrics");
    assert_eq!(ts.granularity, "month");

    // Invalid granularity should 400.
    let bad = server
        .get("/v1/admin/platform/metrics/timeseries?granularity=decade")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(bad.status_code(), 400);

    println!("✅ Platform timeseries works and validates granularity");
}

#[tokio::test]
async fn test_admin_platform_billing_summary() {
    let server = setup_test_server().await;
    let _org = setup_org_with_credits(&server, 5000000000i64).await; // $5.00

    let response = server
        .get("/v1/admin/platform/billing-summary")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200);
    let b: BillingSummary = serde_json::from_str(&response.text()).expect("parse BillingSummary");
    assert!(b.active_paid_credit_limit_usd >= 0.0);
    assert!(b.active_grant_credit_limit_usd >= 0.0);
    assert!(b.total_consumed_usd >= 0.0);

    println!("✅ Platform billing summary works");
}

#[tokio::test]
async fn test_admin_platform_model_revenue() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model_name = setup_qwen_model(&server).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hi model revenue!"}],
            "stream": false,
            "max_tokens": 20
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let response = server
        .get("/v1/admin/platform/model-revenue")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200);
    let report: ModelRevenueReport =
        serde_json::from_str(&response.text()).expect("parse ModelRevenueReport");

    // Sorted by revenue desc; total >= rows returned.
    let eps = 1e-6;
    let mut prev = f64::INFINITY;
    for m in &report.data {
        assert!(
            m.consumed_cost_usd <= prev + eps,
            "models sorted by revenue desc"
        );
        prev = m.consumed_cost_usd;
    }
    assert!(report.total >= report.data.len() as i64);
    assert!(report.total >= 1, "the used model should appear");

    // Pagination: limit=1 returns at most one row but the full total.
    let paged = server
        .get("/v1/admin/platform/model-revenue?limit=1&sort=requests")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(paged.status_code(), 200);
    let paged: ModelRevenueReport =
        serde_json::from_str(&paged.text()).expect("parse ModelRevenueReport");
    assert!(paged.data.len() <= 1);
    assert_eq!(paged.limit, 1);
    assert_eq!(paged.total, report.total);

    println!("✅ Platform model-revenue works, sorts, and paginates");
}

#[tokio::test]
async fn test_admin_platform_org_revenue() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model_name = setup_qwen_model(&server).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hi org revenue!"}],
            "stream": false,
            "max_tokens": 20
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let response = server
        .get("/v1/admin/platform/org-revenue")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200);
    let report: OrgRevenueReport =
        serde_json::from_str(&response.text()).expect("parse OrgRevenueReport");

    // The org that made requests must be attributed; verifiable split reconciles; sorted desc.
    let found = report
        .data
        .iter()
        .find(|o| o.organization_id.to_string() == org.id)
        .expect("org with usage should be attributed");
    assert!(found.requests >= 1, "attributed org should have requests");

    let eps = 1e-6;
    let mut prev = f64::INFINITY;
    for o in &report.data {
        assert!(
            (o.verifiable_consumed_usd + o.non_verifiable_consumed_usd - o.consumed_cost_usd).abs()
                < eps,
            "org {} verifiable+external must equal revenue",
            o.organization_name
        );
        assert!(
            o.consumed_cost_usd <= prev + eps,
            "orgs sorted by revenue desc"
        );
        prev = o.consumed_cost_usd;
    }
    assert!(report.total >= report.data.len() as i64);

    println!("✅ Platform org-revenue works, reconciles, and paginates");
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

    println!("✅ Platform infra summary degrades gracefully");
}

#[tokio::test]
async fn test_admin_platform_analytics_input_validation() {
    let server = setup_test_server().await;
    let sid = get_session_id();

    // Invalid start date -> 400 (not a silent default).
    let r = server
        .get("/v1/admin/platform/metrics?start=not-a-date")
        .add_header("Authorization", format!("Bearer {sid}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(r.status_code(), 400, "invalid start should 400");

    // start >= end -> 400.
    let r = server
        .get("/v1/admin/platform/metrics?start=2026-02-01T00:00:00Z&end=2026-01-01T00:00:00Z")
        .add_header("Authorization", format!("Bearer {sid}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(r.status_code(), 400, "start>=end should 400");

    // Invalid sort -> 400.
    let r = server
        .get("/v1/admin/platform/model-revenue?sort=toknes")
        .add_header("Authorization", format!("Bearer {sid}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(r.status_code(), 400, "invalid sort should 400");

    // Invalid provider_type -> 400.
    let r = server
        .get("/v1/admin/platform/model-revenue?provider_type=banana")
        .add_header("Authorization", format!("Bearer {sid}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(r.status_code(), 400, "invalid provider_type should 400");

    // hour granularity over a huge range -> 400.
    let r = server
        .get("/v1/admin/platform/metrics/timeseries?granularity=hour&start=2020-01-01T00:00:00Z&end=2026-01-01T00:00:00Z")
        .add_header("Authorization", format!("Bearer {sid}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        r.status_code(),
        400,
        "hour granularity over huge range should 400"
    );

    println!("✅ Platform analytics input validation returns 400s");
}

#[tokio::test]
async fn test_admin_platform_model_revenue_offset_beyond_total() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model_name = setup_qwen_model(&server).await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "offset test"}],
            "stream": false,
            "max_tokens": 20
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // A page past the end must still report the true total with empty data
    // (regression: COUNT(*) OVER read from the first row returned 0 here).
    let resp = server
        .get("/v1/admin/platform/model-revenue?limit=10&offset=10000")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(resp.status_code(), 200);
    let report: ModelRevenueReport =
        serde_json::from_str(&resp.text()).expect("parse ModelRevenueReport");
    assert!(report.data.is_empty(), "page beyond end should be empty");
    assert!(
        report.total >= 1,
        "total must reflect matches, not the empty page"
    );

    println!("✅ model-revenue reports correct total on an out-of-range page");
}
