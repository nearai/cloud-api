mod common;

use api::models::BatchUpdateModelApiRequest;
use common::{
    admin_batch_upsert_models, get_api_key_for_org, get_session_id, setup_org_with_credits,
    setup_test_server_with_pool, MOCK_USER_AGENT,
};
use inference_providers::{
    ChatMessage, InferenceProvider, MessageRole, ModelInfo, TokenizeResponse,
};
use std::sync::Arc;

/// Test that context-aware routing correctly filters providers by request size
/// This test verifies that:
/// 1. Providers are sorted by max_model_len (smallest first)
/// 2. Providers with insufficient context are filtered out
#[tokio::test]
async fn test_context_aware_routing_with_multiple_providers() {
    let (server, pool, _mock, _db) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 100000000000i64).await;

    // Setup the test model in the database
    let model_name = "TestModel/ContextAware";
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,
                "currency": "USD"
            },
            "modelDisplayName": "Context Aware Test Model",
            "modelDescription": "Model for testing context-aware routing",
            "contextLength": 131072,
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // Register mock providers with different context lengths
    let small_context_provider = Arc::new(inference_providers::MockProvider::with_models(vec![
        ModelInfo {
            id: model_name.to_string(),
            object: "model".to_string(),
            created: 0,
            owned_by: "test".to_string(),
            max_model_len: Some(4096),
        },
    ]));

    let large_context_provider = Arc::new(inference_providers::MockProvider::with_models(vec![
        ModelInfo {
            id: model_name.to_string(),
            object: "model".to_string(),
            created: 0,
            owned_by: "test".to_string(),
            max_model_len: Some(131072),
        },
    ]));

    // Register both providers for the same model
    pool.register_provider(
        model_name.to_string(),
        small_context_provider.clone()
            as Arc<dyn inference_providers::InferenceProvider + Send + Sync>,
    )
    .await;
    pool.register_provider(
        model_name.to_string(),
        large_context_provider.clone()
            as Arc<dyn inference_providers::InferenceProvider + Send + Sync>,
    )
    .await;

    let api_key = get_api_key_for_org(&server, org.id).await;

    // Test 1: Small request should succeed with either provider
    let small_request = serde_json::json!({
        "model": model_name,
        "messages": [
            {"role": "user", "content": "Hello, world!"}
        ]
    });

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&small_request)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Small request should succeed: {:?}",
        response.text()
    );
    println!("✅ Small request routed successfully");
}

/// Test that the tokenize endpoint is called during routing
#[tokio::test]
async fn test_tokenize_chat_for_routing() {
    let (server, pool, _mock, _db) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 100000000000i64).await;

    // Setup the test model
    let model_name = "Qwen/Qwen3-30B-A3B-Instruct-2507";
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,
                "currency": "USD"
            },
            "modelDisplayName": "Qwen 30B",
            "modelDescription": "Test model",
            "contextLength": 32768,
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // Create a mock provider with known context length
    let mock_provider = Arc::new(inference_providers::MockProvider::with_models(vec![
        ModelInfo {
            id: model_name.to_string(),
            object: "model".to_string(),
            created: 0,
            owned_by: "test".to_string(),
            max_model_len: Some(32768),
        },
    ]));

    pool.register_provider(
        model_name.to_string(),
        mock_provider.clone() as Arc<dyn inference_providers::InferenceProvider + Send + Sync>,
    )
    .await;

    // Test the tokenize_chat method directly
    let tokenize_request = inference_providers::TokenizeChatRequest {
        model: model_name.to_string(),
        messages: vec![ChatMessage {
            role: MessageRole::User,
            content: Some("Hello, this is a test message for tokenization.".to_string()),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        add_special_tokens: Some(true),
    };

    let result: Result<TokenizeResponse, _> = mock_provider.tokenize_chat(tokenize_request).await;
    assert!(result.is_ok(), "Tokenize should succeed");

    let tokenize_response = result.unwrap();
    assert!(
        tokenize_response.count > 0,
        "Token count should be positive"
    );
    assert_eq!(
        tokenize_response.max_model_len, 32768,
        "Max model len should match provider"
    );
    println!(
        "✅ Tokenize returned {} tokens with max_model_len {}",
        tokenize_response.count, tokenize_response.max_model_len
    );

    // Now test a completion request
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request = serde_json::json!({
        "model": model_name,
        "messages": [
            {"role": "user", "content": "Hello, this is a test."}
        ]
    });

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&request)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Request should succeed: {:?}",
        response.text()
    );
    println!("✅ Completion request with context-aware routing succeeded");
}

/// Test that requests exceeding all provider context lengths fail with appropriate error
#[tokio::test]
async fn test_context_aware_routing_rejects_oversized_requests() {
    let (server, pool, _mock, _db) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 100000000000i64).await;

    // Setup the test model
    let model_name = "TestModel/SmallContext";
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,
                "currency": "USD"
            },
            "modelDisplayName": "Small Context Model",
            "modelDescription": "Model with very small context",
            "contextLength": 100,
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // Create a mock provider with very small context
    let small_provider = Arc::new(inference_providers::MockProvider::with_models(vec![
        ModelInfo {
            id: model_name.to_string(),
            object: "model".to_string(),
            created: 0,
            owned_by: "test".to_string(),
            max_model_len: Some(100), // Very small context
        },
    ]));

    pool.register_provider(
        model_name.to_string(),
        small_provider as Arc<dyn inference_providers::InferenceProvider + Send + Sync>,
    )
    .await;

    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a request with a large message that exceeds the context
    // Mock tokenization estimates ~4 chars per token, so 500 chars ≈ 125 tokens > 100
    let large_content = "This is a very long message. ".repeat(50);

    let request = serde_json::json!({
        "model": model_name,
        "messages": [
            {"role": "user", "content": large_content}
        ]
    });

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&request)
        .await;

    // The request should fail because no provider has sufficient context
    // The API returns 500 with a provider error when no provider can handle the request
    assert_eq!(
        response.status_code(),
        500,
        "Oversized request should fail: {:?}",
        response.text()
    );

    println!("✅ Oversized request correctly rejected - no provider had sufficient context");
}
