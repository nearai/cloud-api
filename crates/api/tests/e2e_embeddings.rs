//! E2E tests for embeddings endpoint
//!
//! Tests the /v1/embeddings passthrough endpoint with mock providers.

mod common;

use api::models::{BatchUpdateModelApiRequest, ErrorResponse};
use common::*;

/// Register an embedding model in the database for testing
async fn setup_embedding_model(server: &axum_test::TestServer) -> String {
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "Qwen/Qwen3-Embedding-0.6B".to_string(),
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
            "modelDisplayName": "Qwen3 Embedding",
            "modelDescription": "Qwen3 text embedding model",
            "contextLength": 32768,
            "verifiable": false,
            "isActive": true
        }))
        .unwrap(),
    );
    let updated = admin_batch_upsert_models(server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "Should have updated 1 model");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    "Qwen/Qwen3-Embedding-0.6B".to_string()
}

/// Test basic embeddings functionality
#[tokio::test]
async fn test_embeddings_basic() {
    let server = setup_test_server().await;

    setup_embedding_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/embeddings")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Embedding-0.6B",
            "input": "Hello world"
        }))
        .await;

    assert_eq!(response.status_code(), 200, "Embeddings should succeed");

    let response_json: serde_json::Value = response.json();

    // Verify response structure
    assert_eq!(
        response_json["object"].as_str().unwrap(),
        "list",
        "Response object should be 'list'"
    );
    assert!(
        response_json.get("data").is_some(),
        "Response should have data"
    );
    assert!(
        response_json.get("model").is_some(),
        "Response should have model"
    );
    assert!(
        response_json.get("usage").is_some(),
        "Response should have usage"
    );

    let data = response_json["data"].as_array().unwrap();
    assert!(!data.is_empty(), "Should have at least one embedding");

    // Verify embedding structure
    let embedding = &data[0];
    assert_eq!(embedding["object"].as_str().unwrap(), "embedding");
    assert!(
        embedding["embedding"].is_array(),
        "embedding should be array"
    );
    assert_eq!(embedding["index"].as_i64().unwrap(), 0);

    // Verify usage
    let usage = &response_json["usage"];
    assert!(
        usage["prompt_tokens"].as_i64().unwrap() > 0,
        "prompt_tokens should be positive"
    );
    assert!(
        usage["total_tokens"].as_i64().unwrap() > 0,
        "total_tokens should be positive"
    );
}

/// Test embeddings with array input
#[tokio::test]
async fn test_embeddings_array_input() {
    let server = setup_test_server().await;

    setup_embedding_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/embeddings")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Embedding-0.6B",
            "input": ["Hello world", "Goodbye world"]
        }))
        .await;

    assert_eq!(response.status_code(), 200, "Embeddings should succeed");
}

/// Test model not found
#[tokio::test]
async fn test_embeddings_model_not_found() {
    let server = setup_test_server().await;

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/embeddings")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "NonExistent/Model",
            "input": "Hello world"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        404,
        "Should return 404 for non-existent model"
    );

    let error: ErrorResponse = response.json();
    assert!(
        error.error.message.contains("not found"),
        "Error message should mention model not found"
    );
}

/// Test missing API key
#[tokio::test]
async fn test_embeddings_missing_api_key() {
    let server = setup_test_server().await;

    setup_embedding_model(&server).await;

    let response = server
        .post("/v1/embeddings")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Embedding-0.6B",
            "input": "Hello world"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Should reject request without API key"
    );
}

/// Test invalid request body (missing model field)
#[tokio::test]
async fn test_embeddings_invalid_request() {
    let server = setup_test_server().await;

    setup_embedding_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/embeddings")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "input": "Hello world"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should reject request without model field"
    );
}

/// Test usage costs are deducted
#[tokio::test]
async fn test_embeddings_costs_deducted() {
    let server = setup_test_server().await;

    setup_embedding_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let org_id = org.id.clone();
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Get initial balance
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

    // Make embeddings request
    let response = server
        .post("/v1/embeddings")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Embedding-0.6B",
            "input": "Hello world"
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // Get final balance
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
    let final_spent = final_balance.total_spent;

    // Verify cost was charged
    let actual_cost = final_spent - initial_spent;
    assert!(
        actual_cost > 0,
        "Cost should be greater than 0, got {actual_cost}"
    );

    // Mock provider returns 10 prompt_tokens (see mock.rs embeddings_raw)
    // Cost per token: 1,000,000 nano-dollars
    // Expected cost: 10 * 1,000,000 = 10,000,000 nano-dollars
    let expected_cost = 10_000_000i64;
    let tolerance = 10;

    assert!(
        (actual_cost - expected_cost).abs() <= tolerance,
        "Cost mismatch: expected {expected_cost} nano-dollars (±{tolerance}), got {actual_cost}"
    );
}
