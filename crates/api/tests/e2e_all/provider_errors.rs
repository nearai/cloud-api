// E2E tests for provider error propagation to API clients

use crate::common::*;

use api::models::BatchUpdateModelApiRequest;

/// Helper to create a standard chat completion request body
fn chat_request(model: &str, stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": "Hello"
            }
        ],
        "stream": stream,
        "max_tokens": 10
    })
}

/// Helper to create a standard responses request body
fn responses_request(model: &str, stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "input": "Hello",
        "stream": stream,
        "max_output_tokens": 10
    })
}

// ============================================
// Provider error propagation tests (vLLM-style, is_external: false)
// ============================================

/// Test that a 503 from the provider, after `retry_with_fallback` exhausts
/// every backend, surfaces to the client as HTTP 429 ("Too Many Requests").
/// The error body uses `"type": "service_overloaded"` to distinguish it from
/// per-org rate-limit 429s (`"type": "rate_limit_exceeded"`). 503 stays
/// reserved for narrower per-handler "this dependency is down" cases.
#[tokio::test]
async fn test_provider_error_503_propagated() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Configure mock to return a 503 error
    mock_provider
        .set_error_override(Some(inference_providers::CompletionError::HttpError {
            status_code: 503,
            message: "GPU out of memory".to_string(),
            is_external: false,
        }))
        .await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_request("Qwen/Qwen3-30B-A3B-Instruct-2507", false))
        .await;

    assert_eq!(
        response.status_code(),
        429,
        "Expected 429 (all backends overloaded), got {}",
        response.status_code()
    );

    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "service_overloaded");
    assert!(
        err.error.message.contains("overloaded"),
        "Error message should indicate overloaded status. Got: {}",
        err.error.message
    );
}

/// Test that a 429 error from the provider is propagated as rate_limit_exceeded
#[tokio::test]
async fn test_provider_error_429_propagated() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Configure mock to return a 429 error
    mock_provider
        .set_error_override(Some(inference_providers::CompletionError::HttpError {
            status_code: 429,
            message: "Too many requests".to_string(),
            is_external: false,
        }))
        .await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_request("Qwen/Qwen3-30B-A3B-Instruct-2507", false))
        .await;

    assert_eq!(
        response.status_code(),
        429,
        "Expected 429 Too Many Requests, got {}",
        response.status_code()
    );

    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "rate_limit_exceeded");
}

/// Test that the non-streaming Responses API propagates provider 429s as HTTP 429
/// instead of returning HTTP 200 with status=failed.
#[tokio::test]
async fn test_responses_provider_error_429_propagated_non_streaming() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_error_override(Some(inference_providers::CompletionError::HttpError {
            status_code: 429,
            message: "Too many requests".to_string(),
            is_external: false,
        }))
        .await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&responses_request(
            "Qwen/Qwen3-30B-A3B-Instruct-2507",
            false,
        ))
        .await;

    assert_eq!(
        response.status_code(),
        429,
        "Expected 429 Too Many Requests, got {}",
        response.status_code()
    );

    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "rate_limit_exceeded");
}

/// Test that the non-streaming Responses API propagates 429s emitted by an
/// already-created provider stream instead of treating them as generic stream
/// interruptions.
#[tokio::test]
async fn test_responses_stream_error_429_propagated_non_streaming() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_stream_error_override(Some(inference_providers::CompletionError::HttpError {
            status_code: 429,
            message: "Too many requests".to_string(),
            is_external: false,
        }))
        .await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&responses_request(
            "Qwen/Qwen3-30B-A3B-Instruct-2507",
            false,
        ))
        .await;

    assert_eq!(
        response.status_code(),
        429,
        "Expected 429 Too Many Requests, got {}",
        response.status_code()
    );

    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "rate_limit_exceeded");
}

/// Test that provider stream errors after partial output are still propagated
/// instead of being converted to a completed 200 response with partial content.
#[tokio::test]
async fn test_responses_partial_output_then_stream_error_429_non_streaming() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_default_response(
            inference_providers::mock::ResponseTemplate::new("partial output")
                .with_stream_error_after(
                    1,
                    inference_providers::CompletionError::HttpError {
                        status_code: 429,
                        message: "Too many requests".to_string(),
                        is_external: false,
                    },
                ),
        )
        .await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&responses_request(
            "Qwen/Qwen3-30B-A3B-Instruct-2507",
            false,
        ))
        .await;

    assert_eq!(
        response.status_code(),
        429,
        "Expected 429 Too Many Requests, got {}",
        response.status_code()
    );

    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "rate_limit_exceeded");
}

/// Test that the Responses API returns HTTP 429 with `type=service_overloaded`
/// when all inference backends are exhausted — mirrors the chat completions path
/// in `test_provider_error_503_propagated` and covers the mapping in
/// `crates/services/src/responses/errors.rs`.
#[tokio::test]
async fn test_responses_service_overloaded_returns_429() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_error_override(Some(inference_providers::CompletionError::HttpError {
            status_code: 503,
            message: "GPU out of memory".to_string(),
            is_external: false,
        }))
        .await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&responses_request(
            "Qwen/Qwen3-30B-A3B-Instruct-2507",
            false,
        ))
        .await;

    assert_eq!(
        response.status_code(),
        429,
        "Responses API: expected 429 (all backends overloaded), got {}",
        response.status_code()
    );

    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(
        err.error.r#type, "service_overloaded",
        "Responses API: overloaded error should carry type=service_overloaded, got {}",
        err.error.r#type
    );
}

/// Test that initial inference failures in non-streaming Responses API surface
/// as HTTP errors rather than fallback ResponseObjects with status=failed.
#[tokio::test]
async fn test_responses_missing_model_returns_http_error_non_streaming() {
    let (server, _pool, _mock_provider, _db) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&responses_request(
            "non-existent-model-for-responses-error-test",
            false,
        ))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Expected 400 Bad Request, got {}",
        response.status_code()
    );

    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "invalid_request_error");
    assert_eq!(err.error.param.as_deref(), Some("model"));
    assert!(
        err.error
            .message
            .contains("non-existent-model-for-responses-error-test"),
        "Error should mention the invalid model. Got: {}",
        err.error.message
    );
}

/// Test that a 500 error from the provider is mapped to 502 Bad Gateway
#[tokio::test]
async fn test_provider_error_500_becomes_502() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Configure mock to return a 500 error
    mock_provider
        .set_error_override(Some(inference_providers::CompletionError::HttpError {
            status_code: 500,
            message: "Internal server error".to_string(),
            is_external: false,
        }))
        .await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_request("Qwen/Qwen3-30B-A3B-Instruct-2507", false))
        .await;

    assert_eq!(
        response.status_code(),
        502,
        "Expected 502 Bad Gateway for upstream 500, got {}",
        response.status_code()
    );

    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "bad_gateway");
    assert!(
        !err.error.message.contains("Internal server error"),
        "Error message should not expose provider details. Got: {}",
        err.error.message
    );
    assert!(
        err.error.message.contains("unavailable"),
        "Error message should use generic message. Got: {}",
        err.error.message
    );
}

/// Test that a model configured in DB but not in provider pool returns 400
#[tokio::test]
async fn test_model_not_found_in_provider_returns_400() {
    let (server, _pool, _mock_provider, _db) = setup_test_server_with_pool().await;

    // Register a model in the database that is NOT in the provider pool
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "nonexistent/FakeModel-1B".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 1000000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2000000, "currency": "USD" },
            "modelDisplayName": "Fake Model",
            "modelDescription": "A model that does not exist in any provider",
            "contextLength": 4096,
            "maxOutputLength": 1024,
            "verifiable": false,
            "isActive": true
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_request("nonexistent/FakeModel-1B", false))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Expected 400 for model not in provider pool, got {}",
        response.status_code()
    );

    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "invalid_request_error");
    assert!(
        err.error.message.contains("nonexistent/FakeModel-1B"),
        "Error message should mention the model name. Got: {}",
        err.error.message
    );
}

/// Test that provider error messages are preserved in streaming mode
#[tokio::test]
async fn test_provider_error_message_preserved_in_streaming() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Configure mock to return a 503 error
    mock_provider
        .set_error_override(Some(inference_providers::CompletionError::HttpError {
            status_code: 503,
            message: "Model loading in progress".to_string(),
            is_external: false,
        }))
        .await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_request("Qwen/Qwen3-30B-A3B-Instruct-2507", true))
        .await;

    assert_eq!(
        response.status_code(),
        429,
        "Expected 429 (all backends overloaded) for streaming request, got {}",
        response.status_code()
    );

    let err = response.json::<api::models::ErrorResponse>();
    assert!(
        err.error.message.contains("overloaded"),
        "Error message should indicate overloaded status. Got: {}",
        err.error.message
    );
}

// ============================================
// External provider error mapping tests (is_external: true)
// ============================================

/// Test that a 400 from an external provider is passed through as 400 (client error)
/// External 400 = likely invalid params (context too long, bad format, etc.)
#[tokio::test]
async fn test_external_provider_400_stays_400() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Configure mock to return a 400 error from an external provider
    mock_provider
        .set_error_override(Some(inference_providers::CompletionError::HttpError {
            status_code: 400,
            message: "This model's maximum context length is 131072 tokens".to_string(),
            is_external: true,
        }))
        .await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_request("Qwen/Qwen3-30B-A3B-Instruct-2507", false))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "External provider 400 should stay as 400, got {}",
        response.status_code()
    );

    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "invalid_request_error");
    assert!(
        err.error.message.contains("context length"),
        "Should preserve the client-facing error message. Got: {}",
        err.error.message
    );
}

/// Test that a 400 from vLLM stays as 400 (actual client error)
#[tokio::test]
async fn test_vllm_400_stays_400() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Configure mock to return a 400 error from vLLM (is_external: false)
    mock_provider
        .set_error_override(Some(inference_providers::CompletionError::HttpError {
            status_code: 400,
            message: "Upstream service error".to_string(),
            is_external: false,
        }))
        .await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_request("Qwen/Qwen3-30B-A3B-Instruct-2507", false))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "vLLM 400 should stay as 400, got {}",
        response.status_code()
    );

    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "invalid_request_error");
}
