//! E2E tests for external provider support (OpenAI, Anthropic, Gemini).
//!
//! These tests verify:
//! - Admin API correctly configures external models with provider_type="external"
//! - External models appear in /v1/models with correct metadata
//! - Billing is correctly recorded for external provider completions
//! - External providers don't support TEE attestation
//! - Model updates correctly modify external provider configuration

mod common;

use api::models::BatchUpdateModelApiRequest;
use common::*;

/// Helper to setup an external OpenAI-compatible model via admin API
async fn setup_external_openai_model(server: &axum_test::TestServer) -> String {
    let model_name = "openai/gpt-4o".to_string();
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 2500000,  // $2.50 per million tokens
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 10000000,  // $10.00 per million tokens
                "currency": "USD"
            },
            "modelDisplayName": "GPT-4o",
            "modelDescription": "OpenAI GPT-4o model",
            "contextLength": 128000,
            "verifiable": false,  // External providers are not verifiable
            "isActive": true,
            "providerType": "external",
            "providerConfig": {
                "backend": "openai_compatible",
                "base_url": "https://api.openai.com/v1"
            },
            "attestationSupported": false
        }))
        .unwrap(),
    );

    let updated = admin_batch_upsert_models(server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "Should have created 1 model");
    assert_eq!(
        updated[0].metadata.provider_type, "external",
        "Provider type should be external"
    );
    assert!(
        !updated[0].metadata.attestation_supported,
        "External models should not support attestation"
    );

    // Give the system time to register the provider
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    model_name
}

/// Helper to setup an external Anthropic model via admin API
async fn setup_external_anthropic_model(server: &axum_test::TestServer) -> String {
    let model_name = "anthropic/claude-3-5-sonnet".to_string();
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 3000000,  // $3.00 per million tokens
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 15000000,  // $15.00 per million tokens
                "currency": "USD"
            },
            "modelDisplayName": "Claude 3.5 Sonnet",
            "modelDescription": "Anthropic Claude 3.5 Sonnet model",
            "contextLength": 200000,
            "verifiable": false,
            "isActive": true,
            "providerType": "external",
            "providerConfig": {
                "backend": "anthropic",
                "base_url": "https://api.anthropic.com/v1"
            },
            "attestationSupported": false
        }))
        .unwrap(),
    );

    let updated = admin_batch_upsert_models(server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "Should have created 1 model");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    model_name
}

/// Helper to setup an external Gemini model via admin API
async fn setup_external_gemini_model(server: &axum_test::TestServer) -> String {
    let model_name = "google/gemini-2.0-flash".to_string();
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 75000,  // $0.075 per million tokens
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 300000,  // $0.30 per million tokens
                "currency": "USD"
            },
            "modelDisplayName": "Gemini 2.0 Flash",
            "modelDescription": "Google Gemini 2.0 Flash model",
            "contextLength": 1000000,
            "verifiable": false,
            "isActive": true,
            "providerType": "external",
            "providerConfig": {
                "backend": "gemini",
                "base_url": "https://generativelanguage.googleapis.com/v1beta"
            },
            "attestationSupported": false
        }))
        .unwrap(),
    );

    let updated = admin_batch_upsert_models(server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "Should have created 1 model");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    model_name
}

#[tokio::test]
async fn test_admin_configure_external_openai_model() {
    let (server, _guard) = setup_test_server().await;

    let model_name = setup_external_openai_model(&server).await;

    // Verify the model was created correctly via admin list endpoint
    let response = server
        .get("/v1/admin/models?limit=100")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: api::models::AdminModelListResponse = response.json();

    // Find our model in the list
    let model = list_response
        .models
        .iter()
        .find(|m| m.model_id == model_name)
        .expect("Model should be in list");

    assert_eq!(model.metadata.provider_type, "external");
    assert!(!model.metadata.attestation_supported);
    assert!(!model.metadata.verifiable);

    // Verify provider_config contains expected fields
    let config = model
        .metadata
        .provider_config
        .as_ref()
        .expect("Should have provider_config");
    assert_eq!(config["backend"], "openai_compatible");
    assert_eq!(config["base_url"], "https://api.openai.com/v1");
}

#[tokio::test]
async fn test_admin_configure_external_anthropic_model() {
    let (server, _guard) = setup_test_server().await;

    let model_name = setup_external_anthropic_model(&server).await;

    // Verify the model configuration via list endpoint
    let response = server
        .get("/v1/admin/models?limit=100")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: api::models::AdminModelListResponse = response.json();

    let model = list_response
        .models
        .iter()
        .find(|m| m.model_id == model_name)
        .expect("Model should be in list");

    assert_eq!(model.metadata.provider_type, "external");
    let config = model
        .metadata
        .provider_config
        .as_ref()
        .expect("Should have provider_config");
    assert_eq!(config["backend"], "anthropic");
}

#[tokio::test]
async fn test_admin_configure_external_gemini_model() {
    let (server, _guard) = setup_test_server().await;

    let model_name = setup_external_gemini_model(&server).await;

    // Verify the model configuration via list endpoint
    let response = server
        .get("/v1/admin/models?limit=100")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: api::models::AdminModelListResponse = response.json();

    let model = list_response
        .models
        .iter()
        .find(|m| m.model_id == model_name)
        .expect("Model should be in list");

    assert_eq!(model.metadata.provider_type, "external");
    let config = model
        .metadata
        .provider_config
        .as_ref()
        .expect("Should have provider_config");
    assert_eq!(config["backend"], "gemini");
}

#[tokio::test]
async fn test_external_model_appears_in_models_list() {
    let (server, _guard) = setup_test_server().await;

    let model_name = setup_external_openai_model(&server).await;

    // Create org and get API key
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Fetch models list
    let models = list_models(&server, api_key).await;

    // Find our external model
    let external_model = models.data.iter().find(|m| m.id == model_name);
    assert!(
        external_model.is_some(),
        "External model should appear in models list"
    );

    let model = external_model.unwrap();
    assert_eq!(model.owned_by, "openai"); // Derived from model name prefix

    // Verify pricing is set
    let pricing = model.pricing.as_ref().expect("Should have pricing");
    assert!(pricing.input > 0.0, "Input price should be positive");
    assert!(pricing.output > 0.0, "Output price should be positive");
}

#[tokio::test]
async fn test_external_provider_billing_recorded() {
    let (server, inference_pool, mock_provider, _, _guard) = setup_test_server_with_pool().await;

    // Setup external model
    let model_name = setup_external_openai_model(&server).await;

    // Register mock provider for this external model
    // This simulates what would happen with a real external provider
    let mock_provider_trait: std::sync::Arc<
        dyn inference_providers::InferenceProvider + Send + Sync,
    > = mock_provider.clone();
    inference_pool
        .register_provider(model_name.clone(), mock_provider_trait)
        .await;

    // Create org with credits and get API key
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    // Make a completion request
    let completion_response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hello, world!"}],
            "max_tokens": 50,
            "stream": false
        }))
        .await;

    assert_eq!(
        completion_response.status_code(),
        200,
        "Chat completion should succeed: {}",
        completion_response.text()
    );

    // Extract Inference-Id header
    let inference_id = completion_response
        .headers()
        .get("Inference-Id")
        .expect("Missing Inference-Id header")
        .to_str()
        .unwrap();
    let inference_uuid = uuid::Uuid::parse_str(inference_id).unwrap();

    // Wait for async usage recording
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Query billing costs
    let billing_response = server
        .post("/v1/billing/costs")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "requestIds": [inference_uuid]
        }))
        .await;

    assert_eq!(billing_response.status_code(), 200);

    let body: serde_json::Value = billing_response.json();
    let requests = body["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 1, "Should return 1 cost entry");

    let cost_entry = &requests[0];
    assert!(
        cost_entry["costNanoUsd"].as_i64().unwrap() > 0,
        "External provider completion should have positive cost"
    );
}

#[tokio::test]
async fn test_external_model_non_streaming_completion() {
    let (server, inference_pool, mock_provider, _, _guard) = setup_test_server_with_pool().await;

    // Setup external model
    let model_name = setup_external_openai_model(&server).await;

    // Register mock provider
    let mock_provider_trait: std::sync::Arc<
        dyn inference_providers::InferenceProvider + Send + Sync,
    > = mock_provider.clone();
    inference_pool
        .register_provider(model_name.clone(), mock_provider_trait)
        .await;

    // Create org and get API key
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Make non-streaming completion request
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "Say hello!"}
            ],
            "max_tokens": 100,
            "stream": false,
            "temperature": 0.7
        }))
        .await;

    assert_eq!(response.status_code(), 200, "Completion should succeed");

    // Parse and validate response structure
    let body: serde_json::Value = response.json();

    // Verify response has expected OpenAI-compatible structure
    assert!(body["id"].is_string(), "Response should have id");
    assert_eq!(
        body["object"], "chat.completion",
        "Object should be chat.completion"
    );
    assert!(
        body["created"].is_number(),
        "Response should have created timestamp"
    );
    assert_eq!(
        body["model"], model_name,
        "Model should match requested model"
    );

    // Verify choices
    let choices = body["choices"]
        .as_array()
        .expect("Should have choices array");
    assert!(!choices.is_empty(), "Should have at least one choice");

    let choice = &choices[0];
    assert_eq!(choice["index"], 0, "First choice should have index 0");
    assert!(
        choice["message"]["content"].is_string(),
        "Choice should have message content"
    );
    assert!(
        choice["finish_reason"].is_string() || choice["finish_reason"].is_null(),
        "Should have finish_reason"
    );

    // Verify usage
    let usage = &body["usage"];
    assert!(
        usage["input_tokens"].is_number() || usage["prompt_tokens"].is_number(),
        "Should have input/prompt tokens"
    );
    assert!(
        usage["output_tokens"].is_number() || usage["completion_tokens"].is_number(),
        "Should have output/completion tokens"
    );
}

#[tokio::test]
async fn test_external_model_streaming_completion() {
    let (server, inference_pool, mock_provider, _, _guard) = setup_test_server_with_pool().await;

    // Setup external model
    let model_name = setup_external_openai_model(&server).await;

    // Register mock provider
    let mock_provider_trait: std::sync::Arc<
        dyn inference_providers::InferenceProvider + Send + Sync,
    > = mock_provider.clone();
    inference_pool
        .register_provider(model_name.clone(), mock_provider_trait)
        .await;

    // Create org and get API key
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Make streaming completion request
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Count to 5"}],
            "max_tokens": 50,
            "stream": true
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Streaming completion should succeed"
    );

    // Verify content-type is SSE
    let content_type = response
        .headers()
        .get("content-type")
        .expect("Should have content-type header")
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("text/event-stream"),
        "Content-Type should be text/event-stream for streaming, got: {}",
        content_type
    );

    // Get the response body as text (SSE format)
    let body = response.text();

    // Verify SSE format - should contain data: lines
    assert!(
        body.contains("data:") || body.is_empty(),
        "Streaming response should contain SSE data lines"
    );

    // If we got data, verify it ends with [DONE]
    if !body.is_empty() && body.contains("data:") {
        assert!(
            body.contains("[DONE]"),
            "Streaming response should end with [DONE]"
        );
    }
}

#[tokio::test]
async fn test_external_model_completion_with_anthropic_model() {
    let (server, inference_pool, mock_provider, _, _guard) = setup_test_server_with_pool().await;

    // Setup Anthropic external model
    let model_name = setup_external_anthropic_model(&server).await;

    // Register mock provider (simulating Anthropic backend)
    let mock_provider_trait: std::sync::Arc<
        dyn inference_providers::InferenceProvider + Send + Sync,
    > = mock_provider.clone();
    inference_pool
        .register_provider(model_name.clone(), mock_provider_trait)
        .await;

    // Create org and get API key
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Make completion request
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hello from Anthropic test!"}],
            "max_tokens": 50,
            "stream": false
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Anthropic model completion should succeed: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["model"], model_name);
}

#[tokio::test]
async fn test_external_model_completion_with_gemini_model() {
    let (server, inference_pool, mock_provider, _, _guard) = setup_test_server_with_pool().await;

    // Setup Gemini external model
    let model_name = setup_external_gemini_model(&server).await;

    // Register mock provider (simulating Gemini backend)
    let mock_provider_trait: std::sync::Arc<
        dyn inference_providers::InferenceProvider + Send + Sync,
    > = mock_provider.clone();
    inference_pool
        .register_provider(model_name.clone(), mock_provider_trait)
        .await;

    // Create org and get API key
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Make completion request
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hello from Gemini test!"}],
            "max_tokens": 50,
            "stream": false
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Gemini model completion should succeed: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["model"], model_name);
}

#[tokio::test]
async fn test_external_model_completion_tracks_usage() {
    let (server, inference_pool, mock_provider, _, _guard) = setup_test_server_with_pool().await;

    // Setup external model with known pricing
    let model_name = setup_external_openai_model(&server).await;

    // Register mock provider
    let mock_provider_trait: std::sync::Arc<
        dyn inference_providers::InferenceProvider + Send + Sync,
    > = mock_provider.clone();
    inference_pool
        .register_provider(model_name.clone(), mock_provider_trait)
        .await;

    // Create org with credits and get API key
    let initial_credits: i64 = 10000000000; // $10.00
    let org = setup_org_with_credits(&server, initial_credits).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    // Make multiple completion requests
    for i in 0..3 {
        let response = server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&serde_json::json!({
                "model": model_name,
                "messages": [{"role": "user", "content": format!("Test message {}", i)}],
                "max_tokens": 20,
                "stream": false
            }))
            .await;

        assert_eq!(
            response.status_code(),
            200,
            "Completion {} should succeed",
            i
        );
    }

    // Wait for async usage recording
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // Check organization usage was updated
    let usage_response = server
        .get("/v1/usage")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(usage_response.status_code(), 200);

    let usage: serde_json::Value = usage_response.json();

    // Verify usage was tracked
    let total_tokens = usage["totalTokens"].as_i64().unwrap_or(0);
    assert!(
        total_tokens > 0,
        "Should have recorded token usage after completions"
    );
}

#[tokio::test]
async fn test_external_model_completion_insufficient_credits() {
    let (server, inference_pool, mock_provider, _, _guard) = setup_test_server_with_pool().await;

    // Setup external model
    let model_name = setup_external_openai_model(&server).await;

    // Register mock provider
    let mock_provider_trait: std::sync::Arc<
        dyn inference_providers::InferenceProvider + Send + Sync,
    > = mock_provider.clone();
    inference_pool
        .register_provider(model_name.clone(), mock_provider_trait)
        .await;

    // Create org with ZERO credits
    let org = setup_org_with_credits(&server, 0).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Attempt completion - should fail due to insufficient credits
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "This should fail"}],
            "max_tokens": 50,
            "stream": false
        }))
        .await;

    // Should be rejected (402 Payment Required or 403 Forbidden)
    assert!(
        response.status_code() == 402 || response.status_code() == 403,
        "Should reject completion with no credits, got status: {}",
        response.status_code()
    );
}

#[tokio::test]
async fn test_external_model_cannot_have_attestation_enabled() {
    let (server, _guard) = setup_test_server().await;

    // Try to create an external model with attestation_supported = true
    // The database constraint should prevent this
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "test/invalid-external-model".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,
                "currency": "USD"
            },
            "modelDisplayName": "Invalid External Model",
            "modelDescription": "This should fail",
            "contextLength": 128000,
            "verifiable": false,
            "isActive": true,
            "providerType": "external",
            "providerConfig": {
                "backend": "openai_compatible",
                "base_url": "https://api.example.com/v1"
            },
            "attestationSupported": true  // This should be rejected
        }))
        .unwrap(),
    );

    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&batch)
        .await;

    // Should fail due to database constraint
    assert!(
        response.status_code() == 400 || response.status_code() == 500,
        "Should reject external model with attestation enabled, got status {}",
        response.status_code()
    );
}

#[tokio::test]
async fn test_update_external_provider_config() {
    let (server, _guard) = setup_test_server().await;

    // First create the model
    let model_name = setup_external_openai_model(&server).await;

    // Update the provider config (e.g., change base_url)
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "providerConfig": {
                "backend": "openai_compatible",
                "base_url": "https://api.together.xyz/v1"  // Changed to Together AI
            },
            "changeReason": "Switching to Together AI endpoint"
        }))
        .unwrap(),
    );

    let updated = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1);

    // Verify the update
    let config = updated[0]
        .metadata
        .provider_config
        .as_ref()
        .expect("Should have provider_config");
    assert_eq!(config["base_url"], "https://api.together.xyz/v1");
}

#[tokio::test]
async fn test_deactivate_external_model() {
    let (server, inference_pool, _, _, _guard) = setup_test_server_with_pool().await;

    // Setup and then deactivate an external model
    let model_name = setup_external_openai_model(&server).await;

    // Verify model is initially active
    // External providers are registered but we just verify the model was created
    // The actual provider registration happens asynchronously

    // Deactivate the model
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "isActive": false,
            "changeReason": "Deactivating external model"
        }))
        .unwrap(),
    );

    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&batch)
        .await;

    assert_eq!(response.status_code(), 200);

    // Give time for provider to be unregistered
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Provider should be unregistered
    assert!(
        !inference_pool.is_external_provider(&model_name).await,
        "External provider should be unregistered after deactivation"
    );
}

#[tokio::test]
async fn test_switch_model_from_vllm_to_external() {
    let (server, _guard) = setup_test_server().await;

    // First create a vLLM model (default)
    let model_name = "test/switchable-model".to_string();
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,
                "currency": "USD"
            },
            "modelDisplayName": "Switchable Model",
            "modelDescription": "A model that will be switched to external",
            "contextLength": 128000,
            "verifiable": true,
            "isActive": true,
            "providerType": "vllm",
            "attestationSupported": true
        }))
        .unwrap(),
    );

    let created = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    assert_eq!(created[0].metadata.provider_type, "vllm");
    assert!(created[0].metadata.attestation_supported);

    // Now switch to external
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "providerType": "external",
            "providerConfig": {
                "backend": "openai_compatible",
                "base_url": "https://api.openai.com/v1"
            },
            "attestationSupported": false,  // Must disable attestation for external
            "verifiable": false,
            "changeReason": "Switching to external provider"
        }))
        .unwrap(),
    );

    let updated = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    assert_eq!(updated[0].metadata.provider_type, "external");
    assert!(!updated[0].metadata.attestation_supported);
    assert!(!updated[0].metadata.verifiable);
}

#[tokio::test]
async fn test_batch_create_multiple_external_providers() {
    let (server, _guard) = setup_test_server().await;

    // Create multiple external models in one batch
    let mut batch = BatchUpdateModelApiRequest::new();

    batch.insert(
        "openai/gpt-4o-mini".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 150000, "currency": "USD" },
            "outputCostPerToken": { "amount": 600000, "currency": "USD" },
            "modelDisplayName": "GPT-4o Mini",
            "modelDescription": "OpenAI GPT-4o Mini",
            "contextLength": 128000,
            "verifiable": false,
            "isActive": true,
            "providerType": "external",
            "providerConfig": { "backend": "openai_compatible", "base_url": "https://api.openai.com/v1" },
            "attestationSupported": false
        }))
        .unwrap(),
    );

    batch.insert(
        "anthropic/claude-3-haiku".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 250000, "currency": "USD" },
            "outputCostPerToken": { "amount": 1250000, "currency": "USD" },
            "modelDisplayName": "Claude 3 Haiku",
            "modelDescription": "Anthropic Claude 3 Haiku",
            "contextLength": 200000,
            "verifiable": false,
            "isActive": true,
            "providerType": "external",
            "providerConfig": { "backend": "anthropic", "base_url": "https://api.anthropic.com/v1" },
            "attestationSupported": false
        }))
        .unwrap(),
    );

    batch.insert(
        "google/gemini-1.5-flash".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 75000, "currency": "USD" },
            "outputCostPerToken": { "amount": 300000, "currency": "USD" },
            "modelDisplayName": "Gemini 1.5 Flash",
            "modelDescription": "Google Gemini 1.5 Flash",
            "contextLength": 1000000,
            "verifiable": false,
            "isActive": true,
            "providerType": "external",
            "providerConfig": { "backend": "gemini", "base_url": "https://generativelanguage.googleapis.com/v1beta" },
            "attestationSupported": false
        }))
        .unwrap(),
    );

    let updated = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 3, "Should have created 3 models");

    // Verify all are external
    for model in &updated {
        assert_eq!(model.metadata.provider_type, "external");
        assert!(!model.metadata.attestation_supported);
        assert!(!model.metadata.verifiable);
    }
}

#[tokio::test]
async fn test_external_model_history_records_provider_config() {
    let (server, _guard) = setup_test_server().await;

    // Create an external model
    let model_name = setup_external_openai_model(&server).await;

    // Update it to change the config
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "providerConfig": {
                "backend": "openai_compatible",
                "base_url": "https://api.together.xyz/v1"
            },
            "changeReason": "Changed endpoint"
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // Fetch model history
    let url = format!(
        "/v1/admin/models/{}/history",
        urlencoding::encode(&model_name)
    );
    let response = server
        .get(&url)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let history: Vec<api::models::ModelHistoryEntry> = response.json();

    // Should have at least 2 entries (creation + update)
    assert!(
        history.len() >= 2,
        "Should have history entries for creation and update"
    );
}
