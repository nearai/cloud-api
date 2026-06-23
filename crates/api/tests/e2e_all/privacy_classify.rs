//! E2E tests for /v1/privacy/classify passthrough endpoint
//!
//! Exercises the route end-to-end against the MockProvider, which returns a
//! structurally-valid privacy-filter response with usage.input_tokens=10.

use crate::common::*;
use api::models::{BatchUpdateModelApiRequest, ErrorResponse};

async fn setup_privacy_filter_model(server: &axum_test::TestServer) -> String {
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "openai/privacy-filter".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 0,
                "currency": "USD"
            },
            "costPerImage": {
                "amount": 0,
                "currency": "USD"
            },
            "modelDisplayName": "Privacy Filter",
            "modelDescription": "PII span detection (token classification)",
            "contextLength": 512,
            "maxOutputLength": 1024,
            "verifiable": false,
            "isActive": true
        }))
        .unwrap(),
    );
    let updated = admin_batch_upsert_models(server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "Should have updated 1 model");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    "openai/privacy-filter".to_string()
}

#[tokio::test]
async fn test_privacy_classify_basic() {
    let server = setup_test_server().await;

    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/privacy/classify")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "openai/privacy-filter",
            "input": "My SSN is 123-45-6789",
            "threshold": 0.5
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Privacy classify should succeed"
    );

    let body: serde_json::Value = response.json();
    assert!(
        body.get("model").is_some(),
        "Response should have model field"
    );
    let data = body["data"].as_array().expect("data should be array");
    assert!(!data.is_empty(), "Should have at least one entry");
    assert!(
        data[0].get("spans").is_some(),
        "Each entry should have spans field"
    );
    assert!(
        data[0]["usage"]["input_tokens"].as_i64().unwrap() >= 0,
        "Entry should have non-negative input_tokens"
    );
}

#[tokio::test]
async fn test_privacy_classify_array_input() {
    let server = setup_test_server().await;

    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/privacy/classify")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "openai/privacy-filter",
            "input": ["alice@example.com", "+1-555-123-4567"]
        }))
        .await;

    assert_eq!(response.status_code(), 200);
}

#[tokio::test]
async fn test_privacy_classify_model_not_found() {
    let server = setup_test_server().await;

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/privacy/classify")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "NonExistent/Model",
            "input": "Hello"
        }))
        .await;

    assert_eq!(response.status_code(), 404);
    let error: ErrorResponse = response.json();
    assert!(
        error.error.message.contains("not found"),
        "Error message should mention model not found"
    );
}

#[tokio::test]
async fn test_privacy_classify_missing_api_key() {
    let server = setup_test_server().await;

    setup_privacy_filter_model(&server).await;

    let response = server
        .post("/v1/privacy/classify")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "openai/privacy-filter",
            "input": "Hello"
        }))
        .await;

    assert_eq!(response.status_code(), 401);
}

#[tokio::test]
async fn test_privacy_classify_invalid_request() {
    let server = setup_test_server().await;

    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/privacy/classify")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "input": "Hello"
        }))
        .await;

    assert_eq!(response.status_code(), 400);
}

#[tokio::test]
async fn test_privacy_classify_body_size_limit() {
    let server = setup_test_server().await;

    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // ~300 KB payload, above the 256 KB per-route cap.
    let oversized_input = "x".repeat(300 * 1024);

    let response = server
        .post("/v1/privacy/classify")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "openai/privacy-filter",
            "input": oversized_input,
        }))
        .await;

    assert_eq!(
        response.status_code(),
        413,
        "Should reject body larger than 256 KB cap"
    );
}

#[tokio::test]
async fn test_privacy_classify_costs_deducted() {
    let server = setup_test_server().await;

    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let org_id = org.id.clone();
    let api_key = get_api_key_for_org(&server, org.id).await;

    let initial_balance_response = server
        .get(format!("/v1/organizations/{}/usage/balance", org_id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(initial_balance_response.status_code(), 200);
    let initial_balance = serde_json::from_str::<api::routes::usage::OrganizationBalanceResponse>(
        &initial_balance_response.text(),
    )
    .expect("Failed to parse initial balance");
    let initial_spent = initial_balance.total_spent;

    let response = server
        .post("/v1/privacy/classify")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "openai/privacy-filter",
            "input": "My SSN is 123-45-6789"
        }))
        .await;
    assert_eq!(response.status_code(), 200);

    let final_balance_response = server
        .get(format!("/v1/organizations/{}/usage/balance", org_id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(final_balance_response.status_code(), 200);
    let final_balance = serde_json::from_str::<api::routes::usage::OrganizationBalanceResponse>(
        &final_balance_response.text(),
    )
    .expect("Failed to parse final balance");

    // Mock returns usage.input_tokens=10, price is 1_000_000 USD-9-decimals per token.
    // Expected spend delta = 10 * 1_000_000 = 10_000_000.
    let delta = final_balance.total_spent - initial_spent;
    assert_eq!(
        delta, 10_000_000,
        "Privacy classify should bill 10 tokens × 1_000_000 = 10_000_000",
    );
}
