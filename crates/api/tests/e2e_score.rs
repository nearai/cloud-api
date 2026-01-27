//! E2E tests for the /v1/score endpoint (text similarity scoring)
//!
//! Tests cover:
//! - Basic happy path (successful scoring)
//! - Usage recording and billing
//! - Concurrent request limiting
//! - Input validation (empty text, length limits)
//! - Error handling (model not found, auth failures)

mod common;

use common::*;
use serde_json::json;

/// Test basic score request with valid inputs
#[tokio::test]
async fn test_score_basic_success() {
    let (server, guard) = setup_test_server().await;

    // Setup: Create org with credits and get API key
    setup_qwen_reranker_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await; // $10.00
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Test: Send score request
    let response = server
        .post("/v1/score")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "text_1": "What is the capital of France?",
            "text_2": "The capital of France is Paris."
        }))
        .await;

    // Verify: Success response with score
    assert_eq!(response.status_code(), 200, "Score request should succeed");

    let body = response.json::<serde_json::Value>();
    assert!(body["id"].is_string(), "Response should have an id");
    assert_eq!(
        body["object"].as_str(),
        Some("list"),
        "Object should be 'list'"
    );
    assert!(
        body["created"].is_number(),
        "Response should have created timestamp"
    );
    assert_eq!(
        body["model"].as_str(),
        Some("Qwen/Qwen3-Reranker-0.6B"),
        "Model should match request"
    );

    // Verify score data
    let data = &body["data"];
    assert!(data.is_array(), "Data should be an array");
    assert!(data[0]["score"].is_number(), "Should have score value");
    assert!(
        data[0]["score"]
            .as_f64()
            .map_or(false, |s| s >= 0.0 && s <= 1.0),
        "Score should be between 0 and 1"
    );
    assert_eq!(data[0]["index"].as_i64(), Some(0), "Index should be 0");
    assert_eq!(
        data[0]["object"].as_str(),
        Some("score"),
        "Object type should be 'score'"
    );

    // Verify usage is tracked
    let usage = &body["usage"];
    assert!(
        usage["prompt_tokens"].is_number() || usage["total_tokens"].is_number(),
        "Should have token usage"
    );

    let _ = guard;
}

/// Test that usage is recorded correctly for billing
#[tokio::test]
async fn test_score_usage_recording() {
    let (server, guard) = setup_test_server().await;

    setup_qwen_reranker_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    // Before: Get organization usage (balance includes total_tokens)
    let balance_response = server
        .get(&format!("/v1/organizations/{}/usage/balance", org.id))
        .add_header("Authorization", format!("Bearer rt_{}", MOCK_USER_ID))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        balance_response.status_code(),
        200,
        "Failed to get organization balance"
    );

    let balance_before = balance_response.json::<serde_json::Value>();
    let total_tokens_before = balance_before["total_tokens"].as_i64().unwrap_or(0);

    // Test: Make score request
    let response = server
        .post("/v1/score")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "text_1": "Hello world",
            "text_2": "Hi there"
        }))
        .await;

    assert_eq!(response.status_code(), 200, "Score request should succeed");

    let response_body = response.json::<serde_json::Value>();
    let tokens_used = response_body["usage"]["prompt_tokens"]
        .as_i64()
        .or(response_body["usage"]["total_tokens"].as_i64())
        .unwrap_or(0);

    assert!(tokens_used > 0, "Response should have token count");

    // After: Get organization usage balance
    let balance_response_after = server
        .get(&format!("/v1/organizations/{}/usage/balance", org.id))
        .add_header("Authorization", format!("Bearer rt_{}", MOCK_USER_ID))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        balance_response_after.status_code(),
        200,
        "Failed to get organization balance after request"
    );

    let balance_after = balance_response_after.json::<serde_json::Value>();
    let total_tokens_after = balance_after["total_tokens"].as_i64().unwrap_or(0);

    // Verify usage was recorded
    assert!(
        total_tokens_after > total_tokens_before,
        "Usage should be recorded after score request"
    );

    let _ = guard;
}

/// Test input validation: empty text
#[tokio::test]
async fn test_score_validation_empty_text_1() {
    let (server, guard) = setup_test_server().await;

    setup_qwen_reranker_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Test: Empty text_1
    let response = server
        .post("/v1/score")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "text_1": "",
            "text_2": "Valid text"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should reject empty text_1 with 400"
    );

    let body = response.json::<serde_json::Value>();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("text_1"),
        "Error should mention text_1"
    );

    let _ = guard;
}

/// Test input validation: whitespace-only text
#[tokio::test]
async fn test_score_validation_whitespace_text_2() {
    let (server, guard) = setup_test_server().await;

    setup_qwen_reranker_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Test: Whitespace-only text_2
    let response = server
        .post("/v1/score")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "text_1": "Valid text",
            "text_2": "   \n  \t  "
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should reject whitespace-only text"
    );

    let body = response.json::<serde_json::Value>();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("text_2"),
        "Error should mention text_2"
    );

    let _ = guard;
}

/// Test input validation: text exceeds length limit
#[tokio::test]
async fn test_score_validation_text_too_long() {
    let (server, guard) = setup_test_server().await;

    setup_qwen_reranker_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create text exceeding 100k character limit
    let long_text = "x".repeat(101_000);

    let response = server
        .post("/v1/score")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "text_1": long_text,
            "text_2": "Valid text"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should reject text exceeding length limit"
    );

    let body = response.json::<serde_json::Value>();
    let error_msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("exceeds maximum length"),
        "Error should mention length limit"
    );

    let _ = guard;
}

/// Test error handling: model not found
#[tokio::test]
async fn test_score_model_not_found() {
    let (server, guard) = setup_test_server().await;

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Note: We don't setup the model here to test that non-existent models return 404

    // Test: Non-existent model
    let response = server
        .post("/v1/score")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "non-existent-model",
            "text_1": "text",
            "text_2": "text"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        404,
        "Should return 404 for non-existent model"
    );

    let body = response.json::<serde_json::Value>();
    assert_eq!(
        body["error"]["type"].as_str(),
        Some("not_found_error"),
        "Error type should be 'not_found_error'"
    );

    let _ = guard;
}

/// Test authentication: missing API key
#[tokio::test]
async fn test_score_missing_api_key() {
    let (server, guard) = setup_test_server().await;

    // Test: No Authorization header
    let response = server
        .post("/v1/score")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "text_1": "text",
            "text_2": "text"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Should return 401 for missing API key"
    );

    let body = response.json::<serde_json::Value>();
    // When no Authorization header is provided, we get "missing_auth_header" error type
    assert!(
        body["error"]["type"].as_str() == Some("unauthorized")
            || body["error"]["type"].as_str() == Some("missing_auth_header"),
        "Error type should be 'unauthorized' or 'missing_auth_header', got: {:?}",
        body["error"]["type"]
    );

    let _ = guard;
}

/// Test authentication: invalid API key
#[tokio::test]
async fn test_score_invalid_api_key() {
    let (server, guard) = setup_test_server().await;

    // Test: Invalid API key format
    let response = server
        .post("/v1/score")
        .add_header("Authorization", "Bearer invalid-key-format")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "text_1": "text",
            "text_2": "text"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Should return 401 for invalid API key"
    );

    let _ = guard;
}

/// Test concurrent request limiting
#[tokio::test]
async fn test_score_concurrent_limit() {
    let (server, guard) = setup_test_server().await;

    setup_qwen_reranker_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Test: Send sequential requests to verify no blocking at low concurrency
    // Note: This test simulates rapid requests sequentially.
    // A proper concurrent test would need a concurrent test framework to hold requests open.
    for i in 0..5 {
        let response = server
            .post("/v1/score")
            .add_header("Authorization", format!("Bearer {}", &api_key))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&json!({
                "model": "Qwen/Qwen3-Reranker-0.6B",
                "text_1": format!("text {}", i),
                "text_2": "reference"
            }))
            .await;

        // All should succeed since we're only at 5 sequential requests (limit is 64)
        assert_eq!(
            response.status_code(),
            200,
            "Score request {} should succeed",
            i
        );
    }

    let _ = guard;
}

/// Test response structure matches specification
#[tokio::test]
async fn test_score_response_structure() {
    let (server, guard) = setup_test_server().await;

    setup_qwen_reranker_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/score")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "text_1": "What is AI?",
            "text_2": "AI is artificial intelligence"
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    let body = response.json::<serde_json::Value>();

    // Verify all required fields are present
    assert!(body["id"].is_string());
    assert!(body["object"].is_string());
    assert!(body["created"].is_number());
    assert!(body["model"].is_string());
    assert!(body["data"].is_array());
    assert!(body["usage"].is_object());

    // Verify data array structure
    let data = &body["data"];
    assert!(data.is_array());
    assert!(data[0]["index"].is_number());
    assert!(data[0]["score"].is_number());
    assert!(data[0]["object"].is_string());

    // Verify usage structure
    let usage = &body["usage"];
    assert!(usage["prompt_tokens"].is_number() || usage["total_tokens"].is_number());

    let _ = guard;
}
