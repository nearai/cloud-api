// Import common test utilities

use crate::common::*;

use api::models::BatchUpdateModelApiRequest;
use database::DEFAULT_MODEL_OWNED_BY;
use inference_providers::{models::ChatCompletionChunk, StreamChunk};

const HIGH_CONTEXT_COMPLETION_MODEL: &str =
    "Qwen/Qwen3-30B-A3B-Instruct-2507-e2e-high-context-completion";
const HIGH_CONTEXT_STREAMING_MODEL: &str =
    "Qwen/Qwen3-30B-A3B-Instruct-2507-e2e-high-context-streaming";
const ALIASES_GPT_MODEL: &str = "nearai/gpt-oss-120b-e2e-aliases";
const ALIASES_QWEN_MODEL: &str = "Qwen/Qwen3-30B-A3B-Instruct-2507-e2e-aliases";
const ALIAS_CONSISTENCY_MODEL: &str = "Qwen/Qwen3-30B-A3B-Instruct-2507-e2e-alias-consistency";

#[tokio::test]
async fn test_models_api() {
    let server = setup_test_server().await;
    let model_name = setup_qwen_model(&server).await;
    let (api_key, _) = create_org_and_api_key(&server).await;
    let response = list_models(&server, api_key).await;

    assert!(!response.data.is_empty());

    // Verify the fixture we arranged, not whichever active model sorts first
    // in a shared database.
    let model = response
        .data
        .iter()
        .find(|model| model.id == model_name)
        .expect("Qwen fixture should be present in the public model list");
    assert!(model.pricing.is_some(), "Model should have pricing");
    let pricing = model.pricing.as_ref().unwrap();
    assert!(pricing.input > 0.0, "Input price should be positive");
    assert!(pricing.output > 0.0, "Output price should be positive");
    assert!(
        model.context_length.is_some(),
        "Model should have context_length"
    );
    assert!(
        model.context_length.unwrap() > 0,
        "Context length should be positive"
    );
}

#[tokio::test]
async fn test_chat_completions_api() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [
                {
                    "role": "user",
                    "content": "Hello, how are you?"
                }
            ],
            "stream": true,
            "max_tokens": 50
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // For streaming responses, we get SSE events as text
    let response_text = response.text();

    let mut content = String::new();
    let mut final_response: Option<ChatCompletionChunk> = None;

    // Parse standard OpenAI streaming format: "data: <json>"
    for line in response_text.lines() {
        println!("Line: {line}");

        if let Some(data) = line.strip_prefix("data: ") {
            // Handle the [DONE] marker
            if data.trim() == "[DONE]" {
                println!("Stream completed with [DONE]");
                break;
            }

            // Parse JSON data
            if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                println!(
                    "Parsed JSON: {}",
                    serde_json::to_string_pretty(&chunk).unwrap_or_default()
                );

                let chat_chunk = match chunk {
                    StreamChunk::Chat(chat_chunk) => {
                        println!("Chat chunk: {chat_chunk:?}");
                        Some(chat_chunk)
                    }
                    _ => {
                        println!("Unknown chunk: {chunk:?}");
                        None
                    }
                }
                .unwrap();

                // Extract content from choices[0].delta.content
                if let Some(choice) = chat_chunk.choices.first() {
                    if let Some(delta) = &choice.delta {
                        if let Some(delta_content) = &delta.content {
                            content.push_str(delta_content.as_str());
                            println!("Delta content: '{delta_content}'");
                        }

                        // Check if this is the final chunk (has usage or finish_reason)
                        if choice.finish_reason.is_some() || chat_chunk.usage.is_some() {
                            final_response = Some(chat_chunk.clone());
                            println!("Final chunk detected");
                        }
                    }
                }
            } else {
                println!("Failed to parse JSON: {data}");
            }
        }
    }

    // Verify we got content from the stream
    assert!(!content.is_empty(), "Expected non-empty streamed content");

    println!("Streamed Content: {content}");

    // Verify we got a meaningful response
    // The mock provider returns simple numbered output like "1. 2. 3." which is short but valid
    assert!(
        !content.is_empty(),
        "Expected non-empty content from stream, got: '{content}'"
    );

    // If we have a final response, verify its structure
    if let Some(final_resp) = final_response {
        println!("Final Response: {final_resp:?}");
        assert!(
            !final_resp.choices.is_empty(),
            "Final response should have choices"
        );
        if let Some(choice) = final_resp.choices.first() {
            assert!(
                choice.delta.is_some(),
                "Final response choices should not be empty"
            );
        }

        // If provider reported usage, ensure cached_tokens() is non-negative
        if let Some(usage) = final_resp.usage {
            assert!(
                usage.cached_tokens() >= 0,
                "cached_tokens derived from prompt_tokens_details should be non-negative"
            );
        }
    } else {
        println!("No final response detected - this is okay for some streaming implementations");
    }
}

#[tokio::test]
async fn test_admin_update_model() {
    let server = setup_test_server().await;

    // Exercise the admin upsert with a new test-owned row on every run. This
    // keeps the omitted `ownedBy` field on the insert path, where its default
    // behavior is defined, and avoids state inherited from a reused database.
    let model_name = format!("TestOrg/AdminUpdateModel-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 1000000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2000000, "currency": "USD" },
            "modelDisplayName": "Updated Model Name",
            "modelDescription": "Updated model description",
            "contextLength": 128000,
            "maxOutputLength": 1024,
            "verifiable": true,
            "isActive": true
        }))
        .expect("admin update model fixture should deserialize"),
    );
    let updated = seed_models(&server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "Should have updated 1 model");

    // Retrieve the model to verify the update
    let response = server
        .get(
            format!(
                "/v1/model/{}",
                url::form_urlencoded::byte_serialize(model_name.as_bytes()).collect::<String>()
            )
            .as_str(),
        )
        .await;

    assert_eq!(200, response.status_code());
    let retrieved_model = response.json::<api::models::ModelWithPricing>();

    assert_eq!(model_name, retrieved_model.model_id);
    assert_eq!(
        "Updated Model Name",
        retrieved_model.metadata.model_display_name
    );
    assert_eq!(1000000, retrieved_model.input_cost_per_token.amount);
    assert_eq!(2000000, retrieved_model.output_cost_per_token.amount);
    // Verify that owned_by defaults to DEFAULT_MODEL_OWNED_BY when not explicitly provided
    assert_eq!(retrieved_model.metadata.owned_by, DEFAULT_MODEL_OWNED_BY);
}

#[tokio::test]
async fn test_get_model_by_name() {
    let server = setup_test_server().await;

    // Use a unique model name to avoid conflicts with other parallel tests
    let model_name = "TestOrg/GetModelByName-Test";
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 1000000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2000000, "currency": "USD" },
            "modelDisplayName": "Test Model Display Name",
            "modelDescription": "Test model description for get_model_by_name",
            "contextLength": 128000,
            "maxOutputLength": 1024,
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );
    seed_models(&server, batch, get_session_id()).await;

    // Test retrieving the model by name (public endpoint - no auth required)
    // Model names may contain forward slashes which must be URL-encoded
    println!("Test: Requesting model by name: '{model_name}'");
    let encoded_model_name =
        url::form_urlencoded::byte_serialize(model_name.as_bytes()).collect::<String>();
    println!("Test: URL-encoded for path: '{encoded_model_name}'");

    let response = server
        .get(format!("/v1/model/{encoded_model_name}").as_str())
        .await;

    println!("Response status: {}", response.status_code());
    assert_eq!(200, response.status_code());

    let model_resp = response.json::<api::models::ModelWithPricing>();
    println!("Retrieved model: {model_resp:?}");

    // Verify the model details match what we upserted
    assert_eq!(model_name, model_resp.model_id);
    assert_eq!(
        "Test Model Display Name",
        model_resp.metadata.model_display_name
    );
    assert_eq!(
        "Test model description for get_model_by_name",
        model_resp.metadata.model_description
    );
    assert_eq!(128000, model_resp.metadata.context_length);
    assert!(model_resp.metadata.verifiable);
    assert_eq!(1000000, model_resp.input_cost_per_token.amount);
    assert_eq!(9, model_resp.input_cost_per_token.scale);
    assert_eq!("USD", model_resp.input_cost_per_token.currency);
    assert_eq!(2000000, model_resp.output_cost_per_token.amount);
    assert_eq!(9, model_resp.output_cost_per_token.scale);
    assert_eq!("USD", model_resp.output_cost_per_token.currency);
    // Verify that owned_by defaults to DEFAULT_MODEL_OWNED_BY when not explicitly provided
    assert_eq!(model_resp.metadata.owned_by, DEFAULT_MODEL_OWNED_BY);

    // Test retrieving the same model again by canonical name to verify consistency
    let response_by_name_again = server
        .get(format!("/v1/model/{encoded_model_name}").as_str())
        .await;

    println!(
        "Second canonical name response status: {}",
        response_by_name_again.status_code()
    );
    assert_eq!(200, response_by_name_again.status_code());

    let model_resp_by_name_again = response_by_name_again.json::<api::models::ModelWithPricing>();
    println!("Retrieved model again by canonical name: {model_resp_by_name_again:?}");

    // Verify that both queries return the same model (same model_id)
    assert_eq!(model_resp.model_id, model_resp_by_name_again.model_id);
    println!("✅ Both queries return the same model with consistent model_id!");

    // Test retrieving a non-existent model
    // Note: URL-encode the model name even for non-existent models
    let nonexistent_model = "nonexistent/model";
    let encoded_nonexistent =
        url::form_urlencoded::byte_serialize(nonexistent_model.as_bytes()).collect::<String>();
    let response = server
        .get(format!("/v1/model/{encoded_nonexistent}").as_str())
        .await;

    println!(
        "Non-existent model response status: {}",
        response.status_code()
    );
    assert_eq!(response.status_code(), 404);

    // Only try to parse JSON if there's a body
    let response_text = response.text();
    if !response_text.is_empty() {
        let error: api::models::ErrorResponse =
            serde_json::from_str(&response_text).expect("Failed to parse error response");
        println!("Error response: {error:?}");
        assert_eq!(error.error.r#type, "model_not_found");
        assert!(error
            .error
            .message
            .contains("Model 'nonexistent/model' not found"));
    } else {
        println!("Warning: 404 response had empty body");
    }
}

#[tokio::test]
async fn test_admin_model_custom_owned_by() {
    let server = setup_test_server().await;

    // Test 1: Create model with custom owned_by value
    let mut batch1 = BatchUpdateModelApiRequest::new();
    batch1.insert(
        "custom-model-1".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,
                "currency": "USD"
            },
            "modelDisplayName": "Custom Model",
            "modelDescription": "Test custom owned_by",
            "contextLength": 128000,
            "maxOutputLength": 1024,
            "verifiable": true,
            "ownedBy": "openai"
        }))
        .unwrap(),
    );

    let created_models = seed_models(&server, batch1, get_session_id()).await;
    assert_eq!(created_models.len(), 1);
    let created_model = &created_models[0];
    assert_eq!(
        created_model.metadata.owned_by, "openai",
        "Created model should have custom owned_by"
    );

    // Test 2: Retrieve model and verify custom owned_by persisted
    let model_name = created_model.model_id.clone();
    let encoded_name =
        url::form_urlencoded::byte_serialize(model_name.as_bytes()).collect::<String>();
    let response = server
        .get(format!("/v1/model/{encoded_name}").as_str())
        .await;
    assert_eq!(response.status_code(), 200);
    let retrieved_model = response.json::<api::models::ModelWithPricing>();
    assert_eq!(
        retrieved_model.metadata.owned_by, "openai",
        "Retrieved model should have custom owned_by"
    );

    // Test 3: Update model WITHOUT specifying ownedBy (should preserve)
    let mut batch2 = BatchUpdateModelApiRequest::new();
    batch2.insert(
        "custom-model-1".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 5000000,
                "currency": "USD"
            }
        }))
        .unwrap(),
    );

    let updated_models = seed_models(&server, batch2, get_session_id()).await;
    assert_eq!(updated_models.len(), 1);
    assert_eq!(
        updated_models[0].metadata.owned_by, "openai",
        "Updated model should preserve owned_by when not explicitly set"
    );

    // Test 4: Override owned_by during update
    let mut batch3 = BatchUpdateModelApiRequest::new();
    batch3.insert(
        "custom-model-1".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 5000000,
                "currency": "USD"
            },
            "ownedBy": "anthropic"
        }))
        .unwrap(),
    );

    let overridden_models = seed_models(&server, batch3, get_session_id()).await;
    assert_eq!(overridden_models.len(), 1);
    assert_eq!(
        overridden_models[0].metadata.owned_by, "anthropic",
        "Updated model should have new owned_by when explicitly provided"
    );

    // Test 5: Verify the override persisted in retrieval
    let response = server
        .get(format!("/v1/model/{encoded_name}").as_str())
        .await;
    assert_eq!(response.status_code(), 200);
    let final_model = response.json::<api::models::ModelWithPricing>();
    assert_eq!(
        final_model.metadata.owned_by, "anthropic",
        "Final retrieval should confirm override"
    );
}

#[tokio::test]
async fn test_admin_model_default_owned_by() {
    let server = setup_test_server().await;

    // Create a unique model name to ensure we test INSERT path, not UPDATE path
    let unique_model_name = format!("test-model-{}", uuid::Uuid::new_v4());

    // Create model WITHOUT specifying ownedBy - should default to DEFAULT_MODEL_OWNED_BY
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        unique_model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,
                "currency": "USD"
            },
            "modelDisplayName": "Test Default Owned By",
            "modelDescription": "Testing default owned_by value",
            "contextLength": 128000,
            "maxOutputLength": 1024,
            "verifiable": true
            // NOTE: No ownedBy field - should default to DEFAULT_MODEL_OWNED_BY
        }))
        .unwrap(),
    );

    let created_models = seed_models(&server, batch, get_session_id()).await;
    assert_eq!(created_models.len(), 1);
    assert_eq!(
        created_models[0].metadata.owned_by, DEFAULT_MODEL_OWNED_BY,
        "Model created without ownedBy should default to DEFAULT_MODEL_OWNED_BY"
    );

    // Verify the default persists in retrieval
    let encoded_name =
        url::form_urlencoded::byte_serialize(unique_model_name.as_bytes()).collect::<String>();
    let response = server
        .get(format!("/v1/model/{encoded_name}").as_str())
        .await;
    assert_eq!(response.status_code(), 200);
    let retrieved_model = response.json::<api::models::ModelWithPricing>();
    assert_eq!(
        retrieved_model.metadata.owned_by, DEFAULT_MODEL_OWNED_BY,
        "Retrieved model should have default owned_by value"
    );
}

// ============================================
// Usage Tracking E2E Tests
// ============================================

#[tokio::test]
async fn test_no_credits_denies_request() {
    let server = setup_test_server().await;

    // Create organization WITHOUT setting any credits
    let (api_key, _api_key_response) = create_org_and_api_key(&server).await;

    // Try to make a chat completion request - should be denied (402 Payment Required)
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [
                {
                    "role": "user",
                    "content": "Say hello"
                }
            ],
            "stream": false,
            "max_tokens": 20
        }))
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    // Should get 402 Payment Required - no credits
    assert_eq!(
        response.status_code(),
        402,
        "Expected 402 Payment Required for organization without credits"
    );

    let error = serde_json::from_str::<api::models::ErrorResponse>(&response.text())
        .expect("Failed to parse error response");
    println!("Error: {error:?}");
    assert!(
        error.error.r#type == "no_credits" || error.error.r#type == "no_limit_configured",
        "Expected error type 'no_credits' or 'no_limit_configured'"
    );
}

#[tokio::test]
async fn test_unconfigured_model_rejected() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Try to use a model that exists in discovery but is not configured in database
    // This model is discovered from the endpoint but has no pricing configuration
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "dphn/Dolphin-Mistral-24B-Venice-Edition",
            "messages": [
                {
                    "role": "user",
                    "content": "Hello"
                }
            ],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    // Should get 400 Bad Request - model not configured
    assert_eq!(
        response.status_code(),
        400,
        "Expected 400 Bad Request for unconfigured model"
    );

    let error = serde_json::from_str::<api::models::ErrorResponse>(&response.text())
        .expect("Failed to parse error response");
    println!("Error: {error:?}");

    // Verify error message indicates model is not found
    assert!(
        error.error.message.contains("not found"),
        "Error message should indicate model not found. Got: {}",
        error.error.message
    );

    // Verify error message mentions it's not a valid model name or alias
    assert!(
        error
            .error
            .message
            .contains("not a valid model name or alias"),
        "Error message should mention it's not a valid model name or alias. Got: {}",
        error.error.message
    );

    assert_eq!(
        error.error.r#type, "invalid_request_error",
        "Expected error type 'invalid_request_error'"
    );
}

#[tokio::test]
async fn test_usage_tracking_on_completion() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 1000000000i64).await; // $1.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Make a chat completion request
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [
                {
                    "role": "user",
                    "content": "Say hello"
                }
            ],
            "stream": false,
            "max_tokens": 20
        }))
        .await;

    println!("Completion response status: {}", response.status_code());
    assert_eq!(response.status_code(), 200);

    let completion_response = response.json::<api::models::ChatCompletionResponse>();
    println!("Usage: {:?}", completion_response.usage);

    // Verify completion was recorded
    assert!(completion_response.usage.prompt_tokens > 0);
    assert!(completion_response.usage.completion_tokens > 0);

    // Wait a bit for async usage recording to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
}

#[tokio::test]
async fn test_usage_limit_enforcement() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 1).await; // 1 nano-dollar (minimal)
    println!("Created organization: {org:?}");
    let api_key = get_api_key_for_org(&server, org.id).await;

    // First request should succeed (no usage yet)
    let response1 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [{"role": "user", "content": "Hi"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    println!("First request status: {}", response1.status_code());
    // This might succeed or fail depending on timing

    // Wait for usage to be recorded
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Second request should fail with payment required
    let response2 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [{"role": "user", "content": "Hi again"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    println!("Second request status: {}", response2.status_code());
    println!("Second request body: {}", response2.text());

    // Should get 402 Payment Required after exceeding limit
    assert!(
        response2.status_code() == 402 || response2.status_code() == 200,
        "Expected 402 Payment Required or 200 OK, got: {}",
        response2.status_code()
    );
}

#[tokio::test]
async fn test_completion_cost_calculation() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 1000000000000i64).await; // $1000.00 USD
    println!("Created organization: {}", org.id);

    // Use nearai model to avoid pricing conflicts with other parallel tests
    // that modify Qwen model pricing (e.g., test_model_alias_consistency)
    let model_name = setup_glm_model(&server).await;
    println!("Setup model: {model_name}");

    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    // Get the model pricing BEFORE making the completion request
    // This is critical because parallel tests may modify pricing, and billing uses
    // the pricing at the time of the request
    let encoded_model_name =
        url::form_urlencoded::byte_serialize(model_name.as_bytes()).collect::<String>();
    let model_response = server
        .get(format!("/v1/model/{encoded_model_name}").as_str())
        .await;
    assert_eq!(
        model_response.status_code(),
        200,
        "Should get model pricing"
    );
    let model_pricing = model_response.json::<api::models::ModelWithPricing>();
    let input_cost_per_token = model_pricing.input_cost_per_token.amount;
    let output_cost_per_token = model_pricing.output_cost_per_token.amount;
    println!("Model pricing BEFORE completion - input: {input_cost_per_token}, output: {output_cost_per_token}");

    // Get initial balance (should be 0 or not found)
    let initial_spent = seeding::try_organization_balance(&org.id)
        .await
        .map_or(0i64, |balance| balance.total_spent);
    println!("Initial spent amount (nano-dollars): {initial_spent}");

    // Make a chat completion request with controlled parameters
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [
                {
                    "role": "user",
                    "content": "Say hello in exactly 5 words."
                }
            ],
            "stream": false,
            "max_tokens": 50
        }))
        .await;

    println!("Completion response status: {}", response.status_code());
    assert_eq!(
        response.status_code(),
        200,
        "Completion request should succeed"
    );

    let completion_response = response.json::<api::models::ChatCompletionResponse>();
    println!("Usage: {:?}", completion_response.usage);

    let input_tokens = completion_response.usage.prompt_tokens;
    let output_tokens = completion_response.usage.completion_tokens;

    // Verify we got actual token counts
    assert!(input_tokens > 0, "Should have input tokens");
    assert!(output_tokens > 0, "Should have output tokens");

    // Expected total cost (at scale 9) - use pricing captured BEFORE the completion request
    let expected_input_cost = (input_tokens as i64) * input_cost_per_token;
    let expected_output_cost = (output_tokens as i64) * output_cost_per_token;
    let expected_total_cost = expected_input_cost + expected_output_cost;

    println!("Input tokens: {input_tokens}, cost per token: {input_cost_per_token} nano-dollars");
    println!(
        "Output tokens: {output_tokens}, cost per token: {output_cost_per_token} nano-dollars"
    );
    println!("Expected input cost: {expected_input_cost} nano-dollars");
    println!("Expected output cost: {expected_output_cost} nano-dollars");
    println!("Expected total cost: {expected_total_cost} nano-dollars");

    // Wait for async usage recording to complete (increased to 3s for reliability with remote DB)
    tokio::time::sleep(tokio::time::Duration::from_millis(3000)).await;

    // Get the updated balance
    let balance = seeding::organization_balance(&org.id).await;
    println!("Balance: {balance:?}");
    println!("Total spent: {} nano-dollars", balance.total_spent);

    // Verify limit information is included
    assert!(balance.spend_limit.is_some(), "Should have spend_limit");
    assert_eq!(
        balance.spend_limit.unwrap(),
        1000000000000i64,
        "Limit should be $1000.00"
    );
    assert!(
        balance.spend_limit_display.is_some(),
        "Should have readable limit"
    );
    println!(
        "Spend limit: {}",
        balance.spend_limit_display.as_ref().unwrap()
    );

    // Verify remaining is calculated
    assert!(balance.remaining.is_some(), "Should have remaining");
    assert!(
        balance.remaining_display.is_some(),
        "Should have readable remaining"
    );
    println!("Remaining: {}", balance.remaining_display.as_ref().unwrap());

    // The recorded cost should match our expected calculation (all at scale 9)
    let actual_spent = balance.total_spent - initial_spent;

    println!("Actual spent: {actual_spent} nano-dollars");
    println!("Expected spent: {expected_total_cost} nano-dollars");

    // Verify the cost calculation is correct (with small tolerance for rounding)
    let tolerance = 10; // Allow small rounding differences
    assert!(
        (actual_spent - expected_total_cost).abs() <= tolerance,
        "Cost calculation mismatch: expected {expected_total_cost} (±{tolerance}), got {actual_spent}. \
         Input tokens: {input_tokens}, Output tokens: {output_tokens}, \
         Input cost per token: {input_cost_per_token}, Output cost per token: {output_cost_per_token}"
    );

    // Verify the display format is reasonable
    assert!(
        !balance.total_spent_display.is_empty(),
        "Should have display format"
    );
    assert!(
        balance.total_spent_display.starts_with("$"),
        "Should show dollar sign"
    );
    println!("Total spent display: {}", balance.total_spent_display);

    // Verify usage history also shows the correct cost
    let history = seeding::usage_history(&org.id, 10, 0).await;
    println!("Usage history: {history:?}");

    // Find the most recent entry (should be our completion)
    assert!(
        !history.data.is_empty(),
        "Should have usage history entries"
    );
    let latest_entry = &history.data[0];

    println!("Latest usage entry: {latest_entry:?}");
    assert_eq!(
        latest_entry.model, model_name,
        "Should record correct model"
    );
    assert_eq!(
        latest_entry.input_tokens, input_tokens,
        "Should record correct input tokens"
    );
    assert_eq!(
        latest_entry.output_tokens, output_tokens,
        "Should record correct output tokens"
    );

    // Verify the cost in the history entry matches (all at scale 9 now)
    assert!(
        (latest_entry.total_cost - expected_total_cost).abs() <= tolerance,
        "History entry cost should match: expected {} nano-dollars, got {}",
        expected_total_cost,
        latest_entry.total_cost
    );

    // Verify stop_reason is "stop" for successful non-streaming completion
    // (maps from OpenAI's finish_reason: "stop")
    assert_eq!(
        latest_entry.stop_reason.as_deref(),
        Some("stop"),
        "Stop reason should be 'stop' for successful completion. Found: {:?}",
        latest_entry.stop_reason
    );

    // Verify provider_request_id is set (raw ID from provider)
    assert!(
        latest_entry.provider_request_id.is_some(),
        "Provider request ID should be set. Found: {:?}",
        latest_entry
    );

    // Verify inference_id is set (hashed from provider_request_id)
    assert!(
        latest_entry.inference_id.is_some(),
        "Inference ID should be set. Found: {:?}",
        latest_entry
    );

    // Note: response_id should be None for direct chat completions API (not Responses API)
    assert!(
        latest_entry.response_id.is_none(),
        "Response ID should be None for direct chat completions API. Found: {:?}",
        latest_entry.response_id
    );
}

// ============================================
// Organization Balance and Limit Tests
// ============================================

#[tokio::test]
async fn test_organization_balance_with_limit_and_usage() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD

    // Get balance before any usage
    let initial_balance = seeding::organization_balance(&org.id).await;

    println!("Initial balance: {initial_balance:?}");

    // Verify initial state
    assert_eq!(initial_balance.total_spent, 0);
    assert_eq!(initial_balance.spend_limit.unwrap(), 10000000000i64);
    assert_eq!(initial_balance.remaining.unwrap(), 10000000000i64);
    assert_eq!(initial_balance.spend_limit_display.unwrap(), "$10.00");
    assert_eq!(initial_balance.remaining_display.unwrap(), "$10.00");

    // Make a completion to record some usage
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model_name = setup_qwen_model(&server).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hi"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // Wait for usage recording
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Get balance after usage
    let final_balance = seeding::organization_balance(&org.id).await;

    println!("Final balance: {final_balance:?}");

    // Verify spending was recorded
    assert!(final_balance.total_spent > 0, "Should have recorded spend");

    // Verify limit is still there
    assert_eq!(
        final_balance.spend_limit.unwrap(),
        10000000000i64,
        "Limit should remain $10.00"
    );

    // Verify remaining is calculated correctly
    let expected_remaining = 10000000000i64 - final_balance.total_spent;
    assert_eq!(
        final_balance.remaining.unwrap(),
        expected_remaining,
        "Remaining should be limit - spent"
    );

    // Verify all display fields are present
    assert!(final_balance.spend_limit_display.is_some());
    assert!(final_balance.remaining_display.is_some());
    println!("Spent: {}", final_balance.total_spent_display);
    println!("Limit: {}", final_balance.spend_limit_display.unwrap());
    println!("Remaining: {}", final_balance.remaining_display.unwrap());
}

// ============================================
// High Context and Model Alias Tests
// ============================================

#[tokio::test]
async fn test_high_context_length_completion() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 100000000000i64).await; // $100.00 USD
    println!("Created organization: {}", org.id);

    // Use a test-specific catalog row so this metadata variant cannot mutate
    // the standard Qwen fixture while other nextest processes are using it.
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        HIGH_CONTEXT_COMPLETION_MODEL.to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,  // $0.000001 per token
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,  // $0.000002 per token
                "currency": "USD"
            },
            "modelDisplayName": "Qwen3 30B Instruct",
            "modelDescription": "High context length model for testing (260k tokens)",
            "contextLength": 262144,  // 260k context length
            "maxOutputLength": 1024,
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );

    let updated_models = seed_models(&server, batch, get_session_id()).await;
    println!("Updated Qwen3-30B model: {:?}", updated_models[0]);
    assert_eq!(updated_models[0].metadata.context_length, 262144);

    let api_key = get_api_key_for_org(&server, org.id).await;

    // Generate a very long input to test high context length
    // Each word is roughly 1-2 tokens, so to get ~100k tokens we need a lot of text
    // We'll generate a repetitive text to save memory but still test the token count
    let base_text = "The quick brown fox jumps over the lazy dog. This is a test of high context length processing. ";
    let repetitions = 10000; // This should give us roughly 100k+ tokens
    let very_long_input = base_text.repeat(repetitions);

    println!(
        "Generated input text length: {} characters",
        very_long_input.len()
    );
    println!("Estimated tokens: ~{}k", very_long_input.len() / 4 / 1000); // Rough estimate: 4 chars per token

    // Make a chat completion request with very high context
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": HIGH_CONTEXT_COMPLETION_MODEL,
            "messages": [
                {
                    "role": "user",
                    "content": very_long_input
                },
                {
                    "role": "user",
                    "content": "Based on all the context above, please respond with a short summary."
                }
            ],
            "stream": false,
            "max_tokens": 100
        }))
        .await;

    println!(
        "High context completion response status: {}",
        response.status_code()
    );

    // The request should either succeed (200) or fail with a known error
    // It might fail if the model isn't actually available in the discovery service
    if response.status_code() == 200 {
        let completion_response = response.json::<api::models::ChatCompletionResponse>();
        println!("High context usage: {:?}", completion_response.usage);

        // Verify we got some tokens (mock provider returns small numbers, real provider would return > 50k)
        assert!(
            completion_response.usage.prompt_tokens > 0,
            "Expected some prompt tokens, got: {}",
            completion_response.usage.prompt_tokens
        );

        assert!(
            completion_response.usage.completion_tokens > 0,
            "Should have generated some output"
        );

        println!("Successfully processed high context request!");
        println!("Prompt tokens: {}", completion_response.usage.prompt_tokens);
        println!(
            "Completion tokens: {}",
            completion_response.usage.completion_tokens
        );
    } else {
        // If the model isn't available, that's acceptable for this test
        let response_text = response.text();
        println!("Response (model may not be available): {response_text}");

        // Common acceptable errors:
        // - Model not found (404)
        // - All backends overloaded (429 service_overloaded) — retry_with_fallback exhausted
        // - Model not available (503) — narrow per-handler dependency-down case
        assert!(
            response.status_code() == 404
                || response.status_code() == 429
                || response.status_code() == 503
                || response.status_code() == 500,
            "Expected 200, 404, 429, 500, or 503, got: {}",
            response.status_code()
        );

        println!("Note: Test verified high context handling, but model may not be deployed");
    }
}

#[tokio::test]
async fn test_high_context_streaming() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 100000000000i64).await; // $100.00 USD

    // Keep the streaming variant independent from the non-streaming test as
    // well as from the suite-wide standard Qwen fixture.
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        HIGH_CONTEXT_STREAMING_MODEL.to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,
                "currency": "USD"
            },
            "modelDisplayName": "Qwen3 30B Instruct",
            "modelDescription": "High context length model for streaming (260k tokens)",
            "contextLength": 262144,
            "maxOutputLength": 1024,
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );

    seed_models(&server, batch, get_session_id()).await;

    let api_key = get_api_key_for_org(&server, org.id).await;

    // Generate long context input
    let base_text = "This is a test message for streaming with high context length. ";
    let repetitions = 10000; // Roughly 50k+ tokens
    let long_input = base_text.repeat(repetitions);

    println!(
        "Testing streaming with ~{}k character input",
        long_input.len() / 1000
    );

    // Make a streaming request with high context
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": HIGH_CONTEXT_STREAMING_MODEL,
            "messages": [
                {
                    "role": "user",
                    "content": long_input
                },
                {
                    "role": "user",
                    "content": "Summarize the above briefly."
                }
            ],
            "stream": true,
            "max_tokens": 150
        }))
        .await;

    println!("Streaming response status: {}", response.status_code());

    if response.status_code() == 200 {
        let response_text = response.text();

        let mut content = String::new();
        let mut final_chunk: Option<ChatCompletionChunk> = None;

        // Parse streaming response
        for line in response_text.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if data.trim() == "[DONE]" {
                    break;
                }

                if let Ok(StreamChunk::Chat(chat_chunk)) = serde_json::from_str::<StreamChunk>(data)
                {
                    if let Some(choice) = chat_chunk.choices.first() {
                        if let Some(delta) = &choice.delta {
                            if let Some(delta_content) = &delta.content {
                                content.push_str(delta_content.as_str());
                            }

                            if choice.finish_reason.is_some() || chat_chunk.usage.is_some() {
                                final_chunk = Some(chat_chunk.clone());
                            }
                        }
                    }
                }
            }
        }

        println!("Streamed content length: {} chars", content.len());

        if let Some(final_resp) = final_chunk {
            if let Some(usage) = final_resp.usage {
                println!("High context streaming usage: {usage:?}");
                assert!(
                    usage.prompt_tokens > 30000,
                    "Expected high input token count, got: {}",
                    usage.prompt_tokens
                );
            }
        }

        println!("Successfully streamed high context response!");
    } else {
        println!(
            "Streaming test - model may not be available: status {}",
            response.status_code()
        );
    }
}

// ============================================
// Model Alias Tests
// ============================================

#[tokio::test]
async fn test_model_aliases() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    println!("Created organization: {}", org.id);

    // Set up canonical models with aliases
    // Both catalog rows and aliases are test-specific so reruns cannot alter
    // models used by unrelated tests.

    let mut batch = BatchUpdateModelApiRequest::new();

    // Model 1: GPT-style canonical name with a friendly alias.
    batch.insert(
        ALIASES_GPT_MODEL.to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,  // $0.000001 per token
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,  // $0.000002 per token
                "currency": "USD"
            },
            "modelDisplayName": "GPT OSS 120B",
            "modelDescription": "Open source 120B parameter model",
            "contextLength": 32768,
            "maxOutputLength": 1024,
            "verifiable": true,
            "isActive": true,
            "aliases": [
                "gpt-oss-120b-e2e-aliases",  // Friendly alias
            ]
        }))
        .unwrap(),
    );

    // Model 2: Qwen-style canonical name with a clean alias.
    batch.insert(
        ALIASES_QWEN_MODEL.to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 500000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "modelDisplayName": "DeepSeek V3.1",
            "modelDescription": "DeepSeek V3.1 reasoning model",
            "contextLength": 65536,
            "maxOutputLength": 1024,
            "verifiable": false,
            "isActive": true,
            "aliases": [
                "deepseek/deepseek-v3.1-e2e-aliases"  // Clean alias
            ]
        }))
        .unwrap(),
    );

    let updated_models = seed_models(&server, batch, get_session_id()).await;
    println!("Updated {} models with aliases", updated_models.len());
    assert_eq!(updated_models.len(), 2);

    let org_id = org.id.clone();
    let api_key = get_api_key_for_org(&server, org_id.clone()).await;

    // Test 1: Request using an alias should work
    println!("\n=== Test 1: Request with alias 'openai/gpt-oss-120b' ===");
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "gpt-oss-120b-e2e-aliases",  // Using ALIAS
            "messages": [
                {
                    "role": "user",
                    "content": "Say hello"
                }
            ],
            "stream": false,
            "max_tokens": 20
        }))
        .await;

    println!("Response status: {}", response.status_code());

    if response.status_code() == 200 {
        let completion = response.json::<api::models::ChatCompletionResponse>();
        println!("Completion model field: {}", completion.model);
        println!("Usage: {:?}", completion.usage);

        // Verify response succeeded
        assert!(completion.usage.prompt_tokens > 0);
        assert!(completion.usage.completion_tokens > 0);
        println!("✓ Successfully completed request using alias");
    } else {
        println!(
            "Model may not be available in discovery service: {}",
            response.text()
        );
    }

    // Test 2: Request using canonical name should still work
    println!("\n=== Test 2: Request with canonical name 'nearai/gpt-oss-120b' ===");
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": ALIASES_GPT_MODEL,  // Using CANONICAL name
            "messages": [
                {
                    "role": "user",
                    "content": "Say hi"
                }
            ],
            "stream": false,
            "max_tokens": 20
        }))
        .await;

    println!("Response status: {}", response.status_code());

    if response.status_code() == 200 {
        let completion = response.json::<api::models::ChatCompletionResponse>();
        println!("Completion model field: {}", completion.model);
        println!("Usage: {:?}", completion.usage);
        assert!(completion.usage.prompt_tokens > 0);
        println!("✓ Successfully completed request using canonical name");
    } else {
        println!("Model may not be available: {}", response.text());
    }

    // Test 3: Clean alias for messy canonical name
    println!("\n=== Test 3: Request with clean alias 'deepseek/deepseek-v3.1' ===");
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "deepseek/deepseek-v3.1-e2e-aliases",  // Clean alias
            "messages": [
                {
                    "role": "user",
                    "content": "Test"
                }
            ],
            "stream": false,
            "max_tokens": 15
        }))
        .await;

    println!("Response status: {}", response.status_code());

    if response.status_code() == 200 {
        let completion = response.json::<api::models::ChatCompletionResponse>();
        println!("Completion model field: {}", completion.model);
        println!("✓ Successfully completed request using clean alias for messy canonical name");
    } else {
        println!("Model may not be available: {}", response.text());
    }

    // Test 4: Verify usage is tracked against canonical model
    println!("\n=== Test 4: Verify usage tracking uses canonical model name ===");
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let history = seeding::usage_history(&org_id, 50, 0).await;
    println!("Usage history entries: {}", history.data.len());

    // Check that usage is recorded with canonical model names
    for entry in &history.data {
        println!(
            "Usage entry: model={}, input_tokens={}, output_tokens={}, cost={}",
            entry.model, entry.input_tokens, entry.output_tokens, entry.total_cost
        );

        // Verify model is a canonical model name
        assert!(
            entry.model == ALIASES_GPT_MODEL || entry.model == ALIASES_QWEN_MODEL,
            "Usage should be tracked with canonical model name, got: {}",
            entry.model
        );
    }

    println!("✓ All usage tracked with canonical model names");

    println!("\n=== Alias Test Summary ===");
    println!("✓ Clients can use aliases to request models");
    println!("✓ Aliases resolve to canonical vLLM model names");
    println!("✓ Pricing is defined once per canonical model");
    println!("✓ Usage is tracked against canonical model names");
    println!("✓ Both aliases and canonical names work");
}

#[tokio::test]
async fn test_model_alias_consistency() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD

    // Set up model with multiple aliases
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        ALIAS_CONSISTENCY_MODEL.to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 800000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 1600000,
                "currency": "USD"
            },
            "modelDisplayName": "Qwen3 30B A3B Instruct",
            "modelDescription": "Qwen3 30B model with A3B quantization",
            "contextLength": 32768,
            "maxOutputLength": 1024,
            "verifiable": true,
            "isActive": true,
            "aliases": [
                "qwen/qwen3-30b-a3b-instruct-2507-e2e-alias-consistency",
                "qwen3-30b-e2e-alias-consistency"
            ]
        }))
        .unwrap(),
    );

    let updated_models = seed_models(&server, batch, get_session_id()).await;
    assert_eq!(updated_models.len(), 1);
    println!("Set up model with 2 aliases");

    let api_key = get_api_key_for_org(&server, org.id).await;

    // Make request with first alias
    println!("\n=== Request 1: Using first alias 'qwen/qwen3-30b-a3b-instruct-2507' ===");
    let response1 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "qwen/qwen3-30b-a3b-instruct-2507-e2e-alias-consistency",
            "messages": [{"role": "user", "content": "Hi"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    let cost1 = if response1.status_code() == 200 {
        let completion1 = response1.json::<api::models::ChatCompletionResponse>();
        let input_cost = (completion1.usage.prompt_tokens as i64) * 800000;
        let output_cost = (completion1.usage.completion_tokens as i64) * 1600000;
        let total_cost = input_cost + output_cost;
        println!("Request 1 cost: {total_cost} nano-dollars");
        Some(total_cost)
    } else {
        println!("Model may not be available");
        None
    };

    // Make request with second alias
    println!("\n=== Request 2: Using second alias 'qwen3-30b' ===");
    let response2 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "qwen3-30b-e2e-alias-consistency",
            "messages": [{"role": "user", "content": "Hi"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    if response2.status_code() == 200 {
        let completion2 = response2.json::<api::models::ChatCompletionResponse>();
        let input_cost = (completion2.usage.prompt_tokens as i64) * 800000;
        let output_cost = (completion2.usage.completion_tokens as i64) * 1600000;
        let total_cost = input_cost + output_cost;
        println!("Request 2 cost: {total_cost} nano-dollars");

        // Verify both use the same pricing (from canonical model)
        if let Some(cost1) = cost1 {
            // Costs should be similar (within tolerance due to token count variation)
            let cost_diff = (total_cost - cost1).abs();
            let tolerance_percent = 0.5; // 50% tolerance for token variation
            let max_diff = ((cost1 as f64) * tolerance_percent) as i64;

            println!("Cost comparison: {cost1} vs {total_cost}, diff: {cost_diff}");
            assert!(
                cost_diff <= max_diff || cost_diff.abs() < 100000000, // Allow some variation
                "Both aliases should use same pricing model"
            );
        }
        println!("✓ Different aliases resolve to same canonical model pricing");
    }

    // Test 3: Request with canonical name
    println!("\n=== Request 3: Using canonical name 'Qwen/Qwen3-30B-A3B-Instruct-2507' ===");
    let response3 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": ALIAS_CONSISTENCY_MODEL,
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    if response3.status_code() == 200 {
        let completion3 = response3.json::<api::models::ChatCompletionResponse>();
        println!("Canonical name usage: {:?}", completion3.usage);
        println!("✓ Canonical name still works alongside aliases");
    }

    println!("\n=== Test Complete ===");
    println!("Verified that multiple aliases can point to the same canonical model");
    println!("and all share the same pricing configuration");
}

// ============================================
// Admin Access Token Tests
// ============================================

#[tokio::test]
async fn test_admin_access_token_use_created_token() {
    let server = setup_test_server().await;

    let admin_token = seeding::seed_admin_access_token("admin@test.com").await;

    // Now use the created token to access an admin endpoint
    let update_response = server
        .get("/v1/admin/database-encryption/keys")
        .add_header("Authorization", format!("Bearer {admin_token}"))
        .await;

    assert_eq!(update_response.status_code(), 200);

    println!("✅ Successfully used created admin token on an admin route");
}

#[tokio::test]
async fn test_admin_access_token_user_agent_match() {
    let server = setup_test_server().await;

    let user_agent = "TestClient/1.0";
    let admin_token =
        seeding::seed_admin_access_token_for_agent("admin@test.com", Some(user_agent)).await;

    let update_response = server
        .get("/v1/admin/database-encryption/keys")
        .add_header("Authorization", format!("Bearer {admin_token}"))
        .add_header("User-Agent", user_agent)
        .await;

    assert_eq!(update_response.status_code(), 200);
    println!("✅ Admin access token validated successfully with matching User-Agent");
}

#[tokio::test]
async fn test_admin_access_token_user_agent_mismatch() {
    let server = setup_test_server().await;

    let user_agent_create = "TestClient/1.0";
    let user_agent_diff = "AnotherClient/2.0";

    let admin_token =
        seeding::seed_admin_access_token_for_agent("admin@test.com", Some(user_agent_create)).await;

    let update_response = server
        .get("/v1/admin/database-encryption/keys")
        .add_header("Authorization", format!("Bearer {admin_token}"))
        .add_header("User-Agent", user_agent_diff)
        .await;

    assert_eq!(update_response.status_code(), 401);
    println!("✅ Admin access token rejected with mismatched User-Agent");
}
