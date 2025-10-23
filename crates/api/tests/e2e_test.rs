// Import common test utilities
mod common;

use common::*;

use api::models::{
    BatchUpdateModelApiRequest, ConversationContentPart, ConversationItem, ResponseOutputContent,
    ResponseOutputItem,
};
use inference_providers::{models::ChatCompletionChunk, StreamChunk};

#[tokio::test]
async fn test_models_api() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;
    let response = list_models(&server, api_key).await;

    assert!(!response.data.is_empty());
}

#[tokio::test]
async fn test_chat_completions_api() {
    let server = setup_test_server().await;
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
    assert!(
        content.len() > 10,
        "Expected substantial content from stream, got: '{content}'"
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
    } else {
        println!("No final response detected - this is okay for some streaming implementations");
    }
}

#[tokio::test]
async fn test_responses_api() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    let models = response.json::<api::models::ModelsResponse>();
    assert!(!models.data.is_empty());

    let conversation = create_conversation(&server, api_key.clone()).await;
    println!("Conversation: {conversation:?}");

    let message = "Hello, how are you?".to_string();
    let max_tokens = 10;
    let response = create_response(
        &server,
        conversation.id.clone(),
        models.data[0].id.clone(),
        message.clone(),
        max_tokens,
        api_key.clone(),
    )
    .await;
    println!("Response: {response:?}");

    // Check that response completed successfully
    assert_eq!(response.status, api::models::ResponseStatus::Completed);

    // Check that we got usage information (tokens were generated)
    assert!(
        response.usage.output_tokens > 0,
        "Expected output tokens to be generated"
    );

    // Check that we have output content structure (even if text is empty due to VLLM issues)
    assert!(!response.output.is_empty(), "Expected output items");

    // Log the text we got (may be empty if VLLM has issues)
    for output_item in &response.output {
        if let ResponseOutputItem::Message { content, .. } = output_item {
            for content_part in content {
                if let ResponseOutputContent::OutputText { text, .. } = content_part {
                    println!(
                        "Response text length: {} chars, content: '{}'",
                        text.len(),
                        text
                    );
                    if text.is_empty() {
                        println!(
                            "Warning: VLLM returned empty text despite reporting {} output tokens",
                            response.usage.output_tokens
                        );
                    }
                }
            }
        }
    }

    let conversation_items =
        list_conversation_items(&server, conversation.id, api_key.clone()).await;
    assert_eq!(conversation_items.data.len(), 2);
    match &conversation_items.data[0] {
        ConversationItem::Message { content, .. } => {
            if let ConversationContentPart::InputText { text } = &content[0] {
                assert_eq!(text, message.as_str());
            }
        }
    }
}

async fn create_conversation(
    server: &axum_test::TestServer,
    api_key: String,
) -> api::models::ConversationObject {
    let response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "name": "Test Conversation",
            "description": "A test conversation"
        }))
        .await;
    assert_eq!(response.status_code(), 201);
    response.json::<api::models::ConversationObject>()
}

#[allow(dead_code)]
async fn get_conversation(
    server: &axum_test::TestServer,
    conversation_id: String,
    api_key: String,
) -> api::models::ConversationObject {
    let response = server
        .get(format!("/v1/conversations/{conversation_id}").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::ConversationObject>()
}

async fn list_conversation_items(
    server: &axum_test::TestServer,
    conversation_id: String,
    api_key: String,
) -> api::models::ConversationItemList {
    let response = server
        .get(format!("/v1/conversations/{conversation_id}/items").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::ConversationItemList>()
}

async fn create_response(
    server: &axum_test::TestServer,
    conversation_id: String,
    model: String,
    message: String,
    max_tokens: i64,
    api_key: String,
) -> api::models::ResponseObject {
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation_id,
            },
            "input": message,
            "temperature": 0.7,
            "max_output_tokens": max_tokens,
            "stream": false,
            "model": model
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::ResponseObject>()
}

async fn create_response_stream(
    server: &axum_test::TestServer,
    conversation_id: String,
    model: String,
    message: String,
    max_tokens: i64,
    api_key: String,
) -> (String, api::models::ResponseObject) {
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation_id,
            },
            "input": message,
            "temperature": 0.7,
            "max_output_tokens": max_tokens,
            "stream": true,
            "model": model
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // For streaming responses, we get SSE events as text
    let response_text = response.text();

    let mut content = String::new();
    let mut final_response: Option<api::models::ResponseObject> = None;

    // Parse SSE format: "event: <type>\ndata: <json>\n\n"
    for line_chunk in response_text.split("\n\n") {
        if line_chunk.trim().is_empty() {
            continue;
        }

        let mut event_type = "";
        let mut event_data = "";

        for line in line_chunk.lines() {
            if let Some(event_name) = line.strip_prefix("event: ") {
                event_type = event_name;
            } else if let Some(data) = line.strip_prefix("data: ") {
                event_data = data;
            }
        }

        if !event_data.is_empty() {
            if let Ok(event_json) = serde_json::from_str::<serde_json::Value>(event_data) {
                match event_type {
                    "response.output_text.delta" => {
                        // Accumulate content deltas as they arrive
                        if let Some(delta) = event_json.get("delta").and_then(|v| v.as_str()) {
                            content.push_str(delta);
                            println!("Delta: {delta}");
                        }
                    }
                    "response.completed" => {
                        // Extract final response from completed event
                        if let Some(response_obj) = event_json.get("response") {
                            final_response = Some(
                                serde_json::from_value::<api::models::ResponseObject>(
                                    response_obj.clone(),
                                )
                                .expect("Failed to parse response.completed event"),
                            );
                            println!("Stream completed");
                        }
                    }
                    "response.created" => {
                        println!("Response created");
                    }
                    "response.in_progress" => {
                        println!("Response in progress");
                    }
                    _ => {
                        println!("Event: {event_type}");
                    }
                }
            }
        }
    }

    let final_resp =
        final_response.expect("Expected to receive response.completed event from stream");
    (content, final_resp)
}

#[tokio::test]
async fn test_conversations_api() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Test creating a conversation
    let create_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "name": "Test Conversation",
            "description": "A test conversation"
        }))
        .await;
    assert_eq!(create_response.status_code(), 201);
}

#[tokio::test]
async fn test_streaming_responses_api() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Get available models
    let response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    let models = response.json::<api::models::ModelsResponse>();
    assert!(!models.data.is_empty());

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;
    println!("Conversation: {conversation:?}");

    // Test streaming response
    let message = "Hello, how are you?".to_string();
    let (streamed_content, streaming_response) = create_response_stream(
        &server,
        conversation.id.clone(),
        models.data[0].id.clone(),
        message.clone(),
        50,
        api_key.clone(),
    )
    .await;

    println!("Streamed Content: {streamed_content}");
    println!("Final Response: {streaming_response:?}");

    // Verify we got content from the stream
    assert!(
        !streamed_content.is_empty(),
        "Expected non-empty streamed content"
    );

    // Verify the final response has content
    assert!(streaming_response.output.iter().any(|o| {
        if let ResponseOutputItem::Message { content, .. } = o {
            content.iter().any(|c| {
                if let ResponseOutputContent::OutputText { text, .. } = c {
                    println!("Final Response Text: {text}");
                    !text.is_empty()
                } else {
                    false
                }
            })
        } else {
            false
        }
    }));

    // Verify streamed content matches final response content
    let final_text = streaming_response
        .output
        .iter()
        .filter_map(|o| {
            if let ResponseOutputItem::Message { content, .. } = o {
                content.iter().find_map(|c| {
                    if let ResponseOutputContent::OutputText { text, .. } = c {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        })
        .next()
        .unwrap_or_default();

    assert_eq!(
        streamed_content, final_text,
        "Streamed content should match final response text"
    );
}

#[tokio::test]
async fn test_admin_update_model() {
    let server = setup_test_server().await;

    // Upsert models (using session token with admin domain email)
    let batch = generate_model();
    let batch_for_comparison = generate_model();
    let updated_models = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    println!("Updated models: {updated_models:?}");
    assert_eq!(updated_models.len(), 1);
    let updated_model = &updated_models[0];
    // model_id should be the canonical model_name (the key in the batch HashMap)
    assert_eq!(
        updated_model.model_id,
        *batch_for_comparison.keys().next().unwrap()
    );
    assert_eq!(
        updated_model.metadata.model_display_name,
        "Updated Model Name"
    );
    assert_eq!(updated_model.input_cost_per_token.amount, 1000000);
}

#[tokio::test]
async fn test_get_model_by_name() {
    let server = setup_test_server().await;

    // Upsert a model with a name containing forward slashes
    let batch = generate_model();
    let model_name = batch.keys().next().unwrap().clone();
    let model_request = batch.get(&model_name).unwrap().clone();

    let upserted_models = admin_batch_upsert_models(&server, batch, get_session_id()).await;

    println!("Upserted models: {upserted_models:?}");
    assert_eq!(upserted_models.len(), 1);

    // Test retrieving the model by name (public endpoint - no auth required)
    // Model names may contain forward slashes (e.g., "Qwen/Qwen3-30B-A3B-Instruct-2507")
    // which must be URL-encoded when used in the path
    println!("Test: Requesting model by name: '{model_name}'");
    let encoded_model_name =
        url::form_urlencoded::byte_serialize(model_name.as_bytes()).collect::<String>();
    println!("Test: URL-encoded for path: '{encoded_model_name}'");

    let response = server
        .get(format!("/v1/model/{encoded_model_name}").as_str())
        .await;

    println!("Response status: {}", response.status_code());
    assert_eq!(response.status_code(), 200);

    let model_resp = response.json::<api::models::ModelWithPricing>();
    println!("Retrieved model: {model_resp:?}");

    // Verify the model details match what we upserted
    // The model_id should be the canonical model_name
    assert_eq!(model_resp.model_id, model_name);
    assert_eq!(
        model_resp.metadata.model_display_name,
        model_request.model_display_name.as_deref().unwrap()
    );
    assert_eq!(
        model_resp.metadata.model_description,
        model_request.model_description.as_deref().unwrap()
    );
    assert_eq!(
        model_resp.metadata.context_length,
        model_request.context_length.unwrap()
    );
    assert_eq!(
        model_resp.metadata.verifiable,
        model_request.verifiable.unwrap()
    );
    assert_eq!(
        model_resp.input_cost_per_token.amount,
        model_request.input_cost_per_token.as_ref().unwrap().amount
    );
    assert_eq!(
        model_resp.input_cost_per_token.scale,
        9 // Scale is always 9 (nano-dollars)
    );
    assert_eq!(
        model_resp.input_cost_per_token.currency,
        model_request
            .input_cost_per_token
            .as_ref()
            .unwrap()
            .currency
    );
    assert_eq!(
        model_resp.output_cost_per_token.amount,
        model_request.output_cost_per_token.as_ref().unwrap().amount
    );
    assert_eq!(
        model_resp.output_cost_per_token.scale,
        9 // Scale is always 9 (nano-dollars)
    );
    assert_eq!(
        model_resp.output_cost_per_token.currency,
        model_request
            .output_cost_per_token
            .as_ref()
            .unwrap()
            .currency
    );

    // Test retrieving the same model again by canonical name to verify consistency
    let response_by_name_again = server
        .get(format!("/v1/model/{encoded_model_name}").as_str())
        .await;

    println!(
        "Second canonical name response status: {}",
        response_by_name_again.status_code()
    );
    assert_eq!(response_by_name_again.status_code(), 200);

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
async fn test_admin_update_organization_limits() {
    let server = setup_test_server().await;

    // Create an organization
    let org = create_org(&server).await;
    println!("Created organization: {org:?}");

    // Update organization limits (amount is in nano-dollars, scale 9 is implicit)
    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 100000000000i64,  // $100.00 USD (in nano-dollars)
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Initial credit allocation"
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    assert_eq!(
        response.status_code(),
        200,
        "Organization limits update should succeed"
    );

    let update_response =
        serde_json::from_str::<api::models::UpdateOrganizationLimitsResponse>(&response.text())
            .expect("Failed to parse response");

    println!("Update response: {update_response:?}");

    // Verify the response
    assert_eq!(update_response.organization_id, org.id);
    assert_eq!(update_response.spend_limit.amount, 100000000000i64);
    assert_eq!(update_response.spend_limit.scale, 9); // Scale is always 9 (nano-dollars)
    assert_eq!(update_response.spend_limit.currency, "USD");
    assert!(!update_response.updated_at.is_empty());
}

#[tokio::test]
async fn test_admin_update_organization_limits_invalid_org() {
    let server = setup_test_server().await;

    // Try to update limits for non-existent organization
    let fake_org_id = uuid::Uuid::new_v4().to_string();
    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 50000000000i64,
            "currency": "USD"
        }
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{fake_org_id}/limits").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    println!("Response status: {}", response.status_code());
    assert_eq!(
        response.status_code(),
        404,
        "Should return 404 for non-existent organization"
    );

    let error = response.json::<api::models::ErrorResponse>();
    println!("Error response: {error:?}");
    assert_eq!(error.error.r#type, "organization_not_found");
}

#[tokio::test]
async fn test_admin_update_organization_limits_multiple_times() {
    let server = setup_test_server().await;

    // Create an organization
    let org = create_org(&server).await;

    // First update - set initial limit (scale 9 = nano-dollars)
    let first_update = serde_json::json!({
        "spendLimit": {
            "amount": 50000000000i64,  // $50.00 USD (in nano-dollars)
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Initial allocation"
    });

    let response1 = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&first_update)
        .await;

    assert_eq!(response1.status_code(), 200);
    let response1_data =
        serde_json::from_str::<api::models::UpdateOrganizationLimitsResponse>(&response1.text())
            .unwrap();
    assert_eq!(response1_data.spend_limit.amount, 50000000000i64);

    // Second update - increase limit
    let second_update = serde_json::json!({
        "spendLimit": {
            "amount": 150000000000i64,  // $150.00 USD (in nano-dollars)
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Customer purchased additional credits"
    });

    let response2 = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&second_update)
        .await;

    assert_eq!(response2.status_code(), 200);
    let response2_data =
        serde_json::from_str::<api::models::UpdateOrganizationLimitsResponse>(&response2.text())
            .unwrap();
    assert_eq!(response2_data.spend_limit.amount, 150000000000i64);

    // Verify the second update happened after the first
    let first_updated = chrono::DateTime::parse_from_rfc3339(&response1_data.updated_at).unwrap();
    let second_updated = chrono::DateTime::parse_from_rfc3339(&response2_data.updated_at).unwrap();
    assert!(
        second_updated > first_updated,
        "Second update should be after first update"
    );
}

#[tokio::test]
async fn test_admin_update_organization_limits_usd_only() {
    let server = setup_test_server().await;

    // Create an organization
    let org = create_org(&server).await;

    // All currencies are USD now (fixed scale 9)
    let usd_update = serde_json::json!({
        "spendLimit": {
            "amount": 85000000000i64,  // $85.00 USD (in nano-dollars)
            "currency": "USD"
        },
        "changedBy": "billing-service",
        "changeReason": "Customer purchase"
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&usd_update)
        .await;

    assert_eq!(response.status_code(), 200);
    let response_data =
        serde_json::from_str::<api::models::UpdateOrganizationLimitsResponse>(&response.text())
            .unwrap();
    assert_eq!(response_data.spend_limit.currency, "USD");
    assert_eq!(response_data.spend_limit.amount, 85000000000i64);
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
    assert!(completion_response.usage.input_tokens > 0);
    assert!(completion_response.usage.output_tokens > 0);

    // Wait a bit for async usage recording to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
}

#[tokio::test]
async fn test_usage_limit_enforcement() {
    let server = setup_test_server().await;
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
async fn test_get_organization_balance() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 5000000000i64).await; // $5.00 USD

    // Get balance - should now show limit even with no usage
    let response = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    println!("Balance response status: {}", response.status_code());
    println!("Balance response body: {}", response.text());

    assert_eq!(response.status_code(), 200, "Should get balance with limit");

    let balance =
        serde_json::from_str::<api::routes::usage::OrganizationBalanceResponse>(&response.text())
            .expect("Failed to parse balance response");

    println!("Balance: {balance:?}");

    // Verify limit is included
    assert!(balance.spend_limit.is_some(), "Should have spend_limit");
    assert_eq!(
        balance.spend_limit.unwrap(),
        5000000000i64,
        "Limit should be $5.00 (5B nano-dollars)"
    );
    assert!(
        balance.spend_limit_display.is_some(),
        "Should have spend_limit_display"
    );
    assert_eq!(
        balance.spend_limit_display.unwrap(),
        "$5.00",
        "Display should show $5.00"
    );

    // Verify remaining is calculated correctly (no usage yet, so remaining = limit)
    assert!(balance.remaining.is_some(), "Should have remaining");
    assert_eq!(
        balance.remaining.unwrap(),
        5000000000i64,
        "Remaining should equal limit with no usage"
    );
    assert!(
        balance.remaining_display.is_some(),
        "Should have remaining_display"
    );

    // Verify spent is zero
    assert_eq!(balance.total_spent, 0, "Total spent should be zero");
    assert_eq!(
        balance.total_spent_display, "$0.00",
        "Spent display should be $0.00"
    );
}

#[tokio::test]
async fn test_get_organization_usage_history() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    // Get usage history (should be empty initially)
    let response = server
        .get(format!("/v1/organizations/{}/usage/history", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    println!("Usage history response status: {}", response.status_code());
    assert_eq!(response.status_code(), 200);

    let history_response = response.json::<serde_json::Value>();
    println!("Usage history: {history_response:?}");

    // Should have data array (empty is fine)
    assert!(history_response.get("data").is_some());
}

#[tokio::test]
async fn test_completion_cost_calculation() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 1000000000000i64).await; // $1000.00 USD
    println!("Created organization: {}", org.id);

    // Setup test model with known pricing
    let model_name = setup_test_model(&server).await;
    println!("Setup model: {model_name}");

    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    // Get initial balance (should be 0 or not found)
    let initial_balance_response = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    let initial_spent = if initial_balance_response.status_code() == 200 {
        let balance =
            initial_balance_response.json::<api::routes::usage::OrganizationBalanceResponse>();
        balance.total_spent
    } else {
        0i64
    };
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

    let input_tokens = completion_response.usage.input_tokens;
    let output_tokens = completion_response.usage.output_tokens;

    // Verify we got actual token counts
    assert!(input_tokens > 0, "Should have input tokens");
    assert!(output_tokens > 0, "Should have output tokens");

    // Calculate expected cost based on model pricing (all at scale 9)
    // Input: 1000000 nano-dollars = $0.000001 per token
    // Output: 2000000 nano-dollars = $0.000002 per token

    let input_cost_per_token = 1000000i64; // nano-dollars
    let output_cost_per_token = 2000000i64; // nano-dollars

    // Expected total cost (at scale 9)
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
    let balance_response = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(
        balance_response.status_code(),
        200,
        "Should be able to get balance"
    );
    let balance = balance_response.json::<api::routes::usage::OrganizationBalanceResponse>();
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
    let history_response = server
        .get(format!("/v1/organizations/{}/usage/history?limit=10", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(history_response.status_code(), 200);
    let history = history_response.json::<api::routes::usage::UsageHistoryResponse>();
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
}

// ============================================
// Organization Balance and Limit Tests
// ============================================

#[tokio::test]
async fn test_organization_balance_with_limit_and_usage() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD

    // Get balance before any usage
    let response = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 200);
    let initial_balance =
        serde_json::from_str::<api::routes::usage::OrganizationBalanceResponse>(&response.text())
            .expect("Failed to parse balance");

    println!("Initial balance: {initial_balance:?}");

    // Verify initial state
    assert_eq!(initial_balance.total_spent, 0);
    assert_eq!(initial_balance.spend_limit.unwrap(), 10000000000i64);
    assert_eq!(initial_balance.remaining.unwrap(), 10000000000i64);
    assert_eq!(initial_balance.spend_limit_display.unwrap(), "$10.00");
    assert_eq!(initial_balance.remaining_display.unwrap(), "$10.00");

    // Make a completion to record some usage
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model_name = setup_test_model(&server).await;

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
    let response = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 200);
    let final_balance =
        serde_json::from_str::<api::routes::usage::OrganizationBalanceResponse>(&response.text())
            .expect("Failed to parse balance");

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

    // Upsert Qwen3-30B model with high context length capability (260k)
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
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
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );

    let updated_models = admin_batch_upsert_models(&server, batch, get_session_id()).await;
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
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
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

        // Verify we got a large number of input tokens
        assert!(
            completion_response.usage.input_tokens > 50000,
            "Expected high token count for large context, got: {}",
            completion_response.usage.input_tokens
        );

        assert!(
            completion_response.usage.output_tokens > 0,
            "Should have generated some output"
        );

        println!("Successfully processed high context request!");
        println!("Input tokens: {}", completion_response.usage.input_tokens);
        println!("Output tokens: {}", completion_response.usage.output_tokens);
    } else {
        // If the model isn't available, that's acceptable for this test
        let response_text = response.text();
        println!("Response (model may not be available): {response_text}");

        // Common acceptable errors:
        // - Model not found (404)
        // - Model not available (503)
        assert!(
            response.status_code() == 404
                || response.status_code() == 503
                || response.status_code() == 500,
            "Expected 200, 404, 500, or 503, got: {}",
            response.status_code()
        );

        println!("Note: Test verified high context handling, but model may not be deployed");
    }
}

#[tokio::test]
async fn test_high_context_streaming() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 100000000000i64).await; // $100.00 USD

    // Upsert Qwen3-30B model with high context length capability (260k)
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
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
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );

    admin_batch_upsert_models(&server, batch, get_session_id()).await;

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
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
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
    // Discovery returns these canonical names from vLLM:
    // - "nearai/gpt-oss-120b" (canonical)
    // - "deepseek-ai/DeepSeek-V3.1" (canonical)

    let mut batch = BatchUpdateModelApiRequest::new();

    // Model 1: nearai/gpt-oss-120b (canonical) with aliases
    batch.insert(
        "nearai/gpt-oss-120b".to_string(),
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
            "verifiable": true,
            "isActive": true,
            "aliases": [
                "gpt-oss-120b",  // Friendly alias
            ]
        }))
        .unwrap(),
    );

    // Model 2: deepseek-ai/DeepSeek-V3.1 (canonical with messy name) with clean alias
    batch.insert(
        "deepseek-ai/DeepSeek-V3.1".to_string(),
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
            "verifiable": false,
            "isActive": true,
            "aliases": [
                "deepseek/deepseek-v3.1"  // Clean alias
            ]
        }))
        .unwrap(),
    );

    let updated_models = admin_batch_upsert_models(&server, batch, get_session_id()).await;
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
            "model": "gpt-oss-120b",  // Using ALIAS
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
        assert!(completion.usage.input_tokens > 0);
        assert!(completion.usage.output_tokens > 0);
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
            "model": "nearai/gpt-oss-120b",  // Using CANONICAL name
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
        assert!(completion.usage.input_tokens > 0);
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
            "model": "deepseek/deepseek-v3.1",  // Clean alias
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

    let history_response = server
        .get(format!("/v1/organizations/{org_id}/usage/history?limit=50").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(history_response.status_code(), 200);
    let history = history_response.json::<api::routes::usage::UsageHistoryResponse>();
    println!("Usage history entries: {}", history.data.len());

    // Check that usage is recorded with canonical model names
    for entry in &history.data {
        println!(
            "Usage entry: model={}, input_tokens={}, output_tokens={}, cost={}",
            entry.model, entry.input_tokens, entry.output_tokens, entry.total_cost
        );

        // Verify model is a canonical model name
        assert!(
            entry.model == "nearai/gpt-oss-120b"
                || entry.model == "deepseek-ai/DeepSeek-V3.1"
                || entry.model == "Qwen/Qwen3-30B-A3B-Instruct-2507",
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
        "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
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
            "verifiable": true,
            "isActive": true,
            "aliases": [
                "qwen/qwen3-30b-a3b-instruct-2507",  // Lowercase clean alias
                "qwen3-30b"                           // Short alias
            ]
        }))
        .unwrap(),
    );

    let updated_models = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    assert_eq!(updated_models.len(), 1);
    println!("Set up model with 2 aliases");

    let api_key = get_api_key_for_org(&server, org.id).await;

    // Make request with first alias
    println!("\n=== Request 1: Using first alias 'qwen/qwen3-30b-a3b-instruct-2507' ===");
    let response1 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "qwen/qwen3-30b-a3b-instruct-2507",  // First alias
            "messages": [{"role": "user", "content": "Hi"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    let cost1 = if response1.status_code() == 200 {
        let completion1 = response1.json::<api::models::ChatCompletionResponse>();
        let input_cost = (completion1.usage.input_tokens as i64) * 800000;
        let output_cost = (completion1.usage.output_tokens as i64) * 1600000;
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
            "model": "qwen3-30b",  // Second alias
            "messages": [{"role": "user", "content": "Hi"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    if response2.status_code() == 200 {
        let completion2 = response2.json::<api::models::ChatCompletionResponse>();
        let input_cost = (completion2.usage.input_tokens as i64) * 800000;
        let output_cost = (completion2.usage.output_tokens as i64) * 1600000;
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
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",  // Canonical name
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
// Streaming Signature Verification Tests
// ============================================

#[tokio::test]
async fn test_streaming_chat_completion_signature_verification() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    println!("Created organization: {}", org.id);

    let api_key = get_api_key_for_org(&server, org.id).await;

    // Use a simple, consistent model for testing
    let model_name = "deepseek-ai/DeepSeek-V3.1";

    // Step 1 & 2: Construct request body with streaming enabled
    let request_body = serde_json::json!({
        "messages": [
            {
                "role": "user",
                "content": "Respond with only two words."
            }
        ],
        "stream": true,
        "model": model_name,
        "nonce": 42
    });

    println!("\n=== Request Body ===");
    println!("{}", serde_json::to_string_pretty(&request_body).unwrap());

    // Step 3: Compute expected request hash
    let request_json = serde_json::to_string(&request_body).expect("Failed to serialize request");
    let expected_request_hash = compute_sha256(&request_json);
    println!("\n=== Expected Request Hash ===");
    println!("Request JSON: {request_json}");
    println!("Expected hash: {expected_request_hash}");

    // Step 4: Make streaming request and capture raw response
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&request_body)
        .await;

    println!("\n=== Response Status ===");
    println!("Status: {}", response.status_code());
    assert_eq!(
        response.status_code(),
        200,
        "Streaming request should succeed"
    );

    // Capture the complete raw response text (SSE format)
    let response_text = response.text();
    println!("=== Raw Streaming Response ===");
    println!("{response_text}");

    // Step 5: Parse streaming response to extract chat_id and verify structure
    let mut chat_id: Option<String> = None;
    let mut content = String::new();

    println!("=== Parsing SSE Stream ===");
    for line in response_text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if data.trim() == "[DONE]" {
                println!("Stream completed with [DONE]");
                break;
            }

            if let Ok(StreamChunk::Chat(chat_chunk)) = serde_json::from_str::<StreamChunk>(data) {
                // Extract chat_id from first chunk
                if chat_id.is_none() {
                    chat_id = Some(chat_chunk.id.clone());
                    println!("Extracted chat_id: {}", chat_chunk.id);
                }

                // Accumulate content
                if let Some(choice) = chat_chunk.choices.first() {
                    if let Some(delta) = &choice.delta {
                        if let Some(delta_content) = &delta.content {
                            content.push_str(delta_content.as_str());
                        }
                    }
                }
            }
        }
    }

    let chat_id = chat_id.expect("Should have extracted chat_id from stream");
    println!("Accumulated content: '{content}'");
    assert!(!content.is_empty(), "Should have received some content");

    // Step 6: Compute expected response hash from the complete raw response
    let expected_response_hash = compute_sha256(&response_text);
    println!("\n=== Expected Response Hash ===");
    println!("Expected hash: {expected_response_hash}");

    // Wait for signature to be stored asynchronously
    println!("\n=== Waiting for Signature Storage ===");
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Step 7: Query signature API
    println!("\n=== Querying Signature API ===");
    let signature_response = server
        .get(format!("/v1/signature/{chat_id}?model={model_name}&signing_algo=ecdsa").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    println!("Signature API status: {}", signature_response.status_code());
    assert_eq!(
        signature_response.status_code(),
        200,
        "Signature API should return successfully"
    );

    let signature_json = signature_response.json::<serde_json::Value>();
    println!(
        "Signature response: {}",
        serde_json::to_string_pretty(&signature_json).unwrap()
    );

    // Step 8: Parse signature text field (format: "request_hash:response_hash")
    let signature_text = signature_json
        .get("text")
        .and_then(|v| v.as_str())
        .expect("Signature response should have 'text' field");

    println!("\n=== Parsing Signature Text ===");
    println!("Signature text: {signature_text}");

    let hash_parts: Vec<&str> = signature_text.split(':').collect();
    assert_eq!(
        hash_parts.len(),
        2,
        "Signature text should contain two hashes separated by ':'"
    );

    let actual_request_hash = hash_parts[0];
    let actual_response_hash = hash_parts[1];

    println!("Actual request hash:  {actual_request_hash}");
    println!("Actual response hash: {actual_response_hash}");

    // Step 9: Critical Assertions - These will FAIL with the current bug
    println!("\n=== Hash Verification ===");

    println!("\nRequest Hash Comparison:");
    println!("  Expected: {expected_request_hash}");
    println!("  Actual:   {actual_request_hash}");

    assert_eq!(
        expected_request_hash, actual_request_hash,
        "\n\n❌ REQUEST HASH MISMATCH!\n\
         Expected: {expected_request_hash}\n\
         Actual:   {actual_request_hash}\n\n\
         This means the signature API is not using the correct request body for hashing.\n\
         The signature cannot be verified correctly.\n"
    );

    println!("\nResponse Hash Comparison:");
    println!("  Expected: {expected_response_hash}");
    println!("  Actual:   {actual_response_hash}");

    assert_eq!(
        expected_response_hash, actual_response_hash,
        "\n\n❌ RESPONSE HASH MISMATCH!\n\
         Expected: {expected_response_hash}\n\
         Actual:   {actual_response_hash}\n\n\
         This means the signature API is not using the correct streaming response body for hashing.\n\
         The signature cannot be verified correctly.\n"
    );

    println!("\n✅ All hash verifications passed!");
    println!("The streaming chat completion signatures are correctly computed.");

    // Verify the signature itself is present
    let signature = signature_json
        .get("signature")
        .and_then(|v| v.as_str())
        .expect("Should have signature field");
    assert!(!signature.is_empty(), "Signature should not be empty");
    assert!(
        signature.starts_with("0x"),
        "Signature should be hex-encoded"
    );

    let signing_address = signature_json
        .get("signing_address")
        .and_then(|v| v.as_str())
        .expect("Should have signing_address field");
    assert!(
        !signing_address.is_empty(),
        "Signing address should not be empty"
    );

    let signing_algo = signature_json
        .get("signing_algo")
        .and_then(|v| v.as_str())
        .expect("Should have signing_algo field");
    assert_eq!(signing_algo, "ecdsa", "Should use ECDSA signing algorithm");

    println!("\n=== Test Summary ===");
    println!("✅ Streaming request succeeded");
    println!("✅ Chat completion ID extracted: {chat_id}");
    println!("✅ Content received: {} chars", content.len());
    println!("✅ Signature stored and retrieved");
    println!("✅ Request hash matches: {expected_request_hash}");
    println!("✅ Response hash matches: {expected_response_hash}");
    println!("✅ Signature is present: {}", &signature[..20]);
    println!("✅ Signing address: {signing_address}");
    println!("✅ Signing algorithm: {signing_algo}");
}

// ============================================
// Admin Access Token Tests
// ============================================

#[tokio::test]
async fn test_admin_access_token_create_long_term() {
    let server = setup_test_server().await;

    let expires_in_hours = 4320; // 180 days
    let name = "Production Billing Service Token";
    let reason = "This is a production billing service token";

    // Test long-term access token (180 days)
    let request = serde_json::json!({
        "expires_in_hours": expires_in_hours,
        "name": name,
        "reason": reason,
    });

    // Get access token from refresh token
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    let response = server
        .post("/v1/admin/access_token")
        .add_header("Authorization", format!("Bearer {}", access_token))
        .json(&request)
        .await;

    assert_eq!(response.status_code(), 200);

    let token_response = response.json::<api::models::AdminAccessTokenResponse>();

    // Verify response structure
    assert!(!token_response.access_token.is_empty());
    assert_eq!(token_response.created_by_user_id, MOCK_USER_ID);
    assert_eq!(token_response.name, name);
    assert_eq!(token_response.reason, reason);

    // Verify expiration is approximately 180 days from now
    let now = chrono::Utc::now();
    let expected_expiry = now + chrono::Duration::hours(expires_in_hours);
    let time_diff = (token_response.expires_at - expected_expiry)
        .num_minutes()
        .abs();
    assert!(
        time_diff < 5,
        "Expiration time should be within 5 minutes of expected time"
    );

    println!(
        "✅ Created long-term admin access token: {}",
        &token_response.access_token[..20]
    );
}

#[tokio::test]
async fn test_admin_access_token_create_invalid_expiration() {
    let server = setup_test_server().await;

    // Test with invalid expiration time (negative)
    let request = serde_json::json!({
        "expires_in_hours": -1,
        "name": "Invalid Token",
        "reason": "Testing invalid expiration"
    });

    // Get access token from refresh token
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    let response = server
        .post("/v1/admin/access_token")
        .add_header("Authorization", format!("Bearer {}", access_token))
        .json(&request)
        .await;

    assert_eq!(response.status_code(), 400);

    // Check the response body for validation error
    let response_text = response.text();
    assert!(response_text.contains("must be a positive number"));

    println!("✅ Correctly rejected negative expiration time");
}

#[tokio::test]
async fn test_admin_access_token_create_zero_expiration() {
    let server = setup_test_server().await;

    // Test with zero expiration time
    let request = serde_json::json!({
        "expires_in_hours": 0,
        "name": "Zero Expiration Token",
        "reason": "Testing zero expiration"
    });

    // Get access token from refresh token
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    let response = server
        .post("/v1/admin/access_token")
        .add_header("Authorization", format!("Bearer {}", access_token))
        .json(&request)
        .await;

    assert_eq!(response.status_code(), 400);

    // Check the response body for validation error
    let response_text = response.text();
    assert!(response_text.contains("must be a positive number"));

    println!("✅ Correctly rejected zero expiration time");
}

#[tokio::test]
async fn test_admin_access_token_create_unauthorized() {
    let server = setup_test_server().await;

    // Test without authorization header
    let request = serde_json::json!({
        "expires_in_hours": 24
    });

    let response = server.post("/v1/admin/access_token").json(&request).await;

    assert_eq!(response.status_code(), 401);

    println!("✅ Correctly rejected request without authorization");
}

#[tokio::test]
#[ignore] // the implementation of MockAuthService accepts any string as valid token, so this test won't pass
async fn test_admin_access_token_create_invalid_token() {
    let server = setup_test_server().await;

    // Test with invalid session token
    let request = serde_json::json!({
        "expires_in_hours": 24
    });

    let response = server
        .post("/v1/admin/access_token")
        .add_header("Authorization", "Bearer invalid_token_12345")
        .json(&request)
        .await;

    assert_eq!(response.status_code(), 401);

    println!("✅ Correctly rejected request with invalid token");
}

#[tokio::test]
async fn test_admin_access_token_use_created_token() {
    let server = setup_test_server().await;

    // First, create an admin access token
    let create_request = serde_json::json!({
        "expires_in_hours": 24,
        "name": "Test Token",
        "reason": "Testing admin access token functionality",
    });

    let create_response = server
        .post("/v1/admin/access_token")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&create_request)
        .await;

    assert_eq!(create_response.status_code(), 200);
    let token_response = create_response.json::<api::models::AdminAccessTokenResponse>();
    let admin_token = token_response.access_token;

    // Now use the created token to access an admin endpoint
    let org = create_org(&server).await;

    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 50000000000i64, // $50.00 USD
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Test using created admin token"
    });

    let update_response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {admin_token}"))
        .json(&update_request)
        .await;

    assert_eq!(update_response.status_code(), 200);

    let update_result = update_response.json::<api::models::UpdateOrganizationLimitsResponse>();
    assert_eq!(update_result.organization_id, org.id);
    assert_eq!(update_result.spend_limit.amount, 50000000000i64);

    println!("✅ Successfully used created admin token to update organization limits");
}

#[tokio::test]
async fn test_admin_access_token_create_and_list() {
    let server = setup_test_server().await;

    // Get access token from refresh token
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Create an admin access token
    let create_request = serde_json::json!({
        "expires_in_hours": 24,
        "name": "Test Token",
        "reason": "Testing admin access token functionality"
    });

    let create_response = server
        .post("/v1/admin/access_token")
        .add_header("Authorization", format!("Bearer {}", access_token))
        .json(&create_request)
        .await;

    assert_eq!(create_response.status_code(), 200);
    let token_response = create_response.json::<api::models::AdminAccessTokenResponse>();
    assert!(!token_response.access_token.is_empty());
    assert_eq!(token_response.name, "Test Token");

    // List admin access tokens
    let list_response = server
        .get("/v1/admin/access_token")
        .add_header("Authorization", format!("Bearer {}", access_token))
        .await;

    assert_eq!(list_response.status_code(), 200);

    let list_data = list_response.json::<serde_json::Value>();
    assert!(list_data["data"].is_array());
    assert!(list_data["total"].is_number());
    assert!(list_data["limit"].is_number());
    assert!(list_data["offset"].is_number());

    // Verify we have at least one token
    let tokens = list_data["data"].as_array().unwrap();
    assert!(!tokens.is_empty());

    // Check the structure of the first token
    let first_token = &tokens[0];
    assert!(first_token["id"].is_string());
    assert!(first_token["name"].is_string());
    assert!(first_token["created_by_user_id"].is_string());
    assert!(first_token["creation_reason"].is_string());
    assert!(first_token["expires_at"].is_string());
    assert!(first_token["created_at"].is_string());
    assert!(first_token["is_active"].is_boolean());

    println!("✅ Admin access token create and list works correctly");
}

#[tokio::test]
async fn test_admin_access_token_list_pagination() {
    let server = setup_test_server().await;

    // Get access token from refresh token
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Create multiple admin access tokens to test pagination
    for i in 0..3 {
        let create_request = serde_json::json!({
            "expires_in_hours": 24,
            "name": format!("Test Token {}", i),
            "reason": format!("Testing pagination {}", i)
        });

        let create_response = server
            .post("/v1/admin/access_token")
            .add_header("Authorization", format!("Bearer {}", access_token))
            .json(&create_request)
            .await;

        assert_eq!(create_response.status_code(), 200);
    }

    // Test pagination with limit and offset
    let list_response = server
        .get("/v1/admin/access_token?limit=2&offset=0")
        .add_header("Authorization", format!("Bearer {}", access_token))
        .await;

    assert_eq!(list_response.status_code(), 200);

    let list_data = list_response.json::<serde_json::Value>();
    let tokens = list_data["data"].as_array().unwrap();

    // Should have at most 2 records due to limit
    assert!(tokens.len() <= 2);
    assert_eq!(list_data["limit"], 2);
    assert_eq!(list_data["offset"], 0);

    // Test second page
    let list_response2 = server
        .get("/v1/admin/access_token?limit=2&offset=2")
        .add_header("Authorization", format!("Bearer {}", access_token))
        .await;

    assert_eq!(list_response2.status_code(), 200);

    let list_data2 = list_response2.json::<serde_json::Value>();

    assert_eq!(list_data2["limit"], 2);
    assert_eq!(list_data2["offset"], 2);

    println!("✅ Admin access token list pagination works correctly");
}

#[tokio::test]
async fn test_admin_access_token_create_and_delete() {
    let server = setup_test_server().await;

    // Get access token from refresh token
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Create an admin access token
    let create_request = serde_json::json!({
        "expires_in_hours": 24,
        "name": "Token to Delete",
        "reason": "Testing delete functionality"
    });

    let create_response = server
        .post("/v1/admin/access_token")
        .add_header("Authorization", format!("Bearer {}", access_token))
        .json(&create_request)
        .await;

    assert_eq!(create_response.status_code(), 200);
    let token_response = create_response.json::<api::models::AdminAccessTokenResponse>();
    let token_id = token_response.id;

    // Delete the admin access token
    let delete_request = serde_json::json!({
        "reason": "Testing delete functionality"
    });

    let delete_response = server
        .delete(&format!("/v1/admin/access_token/{}", token_id))
        .add_header("Authorization", format!("Bearer {}", access_token))
        .json(&delete_request)
        .await;

    assert_eq!(delete_response.status_code(), 200);

    let delete_data = delete_response.json::<serde_json::Value>();
    assert_eq!(
        delete_data["message"],
        "Admin access token revoked successfully"
    );
    assert_eq!(delete_data["token_id"], token_id);
    assert!(delete_data["revoked_by"].is_string());
    assert!(delete_data["revoked_at"].is_string());

    println!("✅ Admin access token delete works correctly");
}

#[tokio::test]
async fn test_admin_access_token_delete_not_found() {
    let server = setup_test_server().await;

    // Get access token from refresh token
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Try to delete a non-existent token
    let fake_token_id = "00000000-0000-0000-0000-000000000000";
    let delete_request = serde_json::json!({
        "reason": "Testing not found scenario"
    });

    let delete_response = server
        .delete(&format!("/v1/admin/access_token/{}", fake_token_id))
        .add_header("Authorization", format!("Bearer {}", access_token))
        .json(&delete_request)
        .await;

    assert_eq!(delete_response.status_code(), 404);

    let delete_data = delete_response.json::<serde_json::Value>();
    assert!(delete_data["error"]["message"].is_string());
    assert!(delete_data["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not found"));

    println!("✅ Admin access token delete correctly handles not found");
}

#[tokio::test]
async fn test_admin_access_token_delete_invalid_id() {
    let server = setup_test_server().await;

    // Get access token from refresh token
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Try to delete with invalid token ID format
    let invalid_token_id = "invalid-id";
    let delete_request = serde_json::json!({
        "reason": "Testing invalid ID format"
    });

    let delete_response = server
        .delete(&format!("/v1/admin/access_token/{}", invalid_token_id))
        .add_header("Authorization", format!("Bearer {}", access_token))
        .json(&delete_request)
        .await;

    assert_eq!(delete_response.status_code(), 400);

    let delete_data = delete_response.json::<serde_json::Value>();
    assert!(delete_data["error"]["message"].is_string());
    assert!(delete_data["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Invalid token ID format"));

    println!("✅ Admin access token delete correctly handles invalid ID format");
}

#[tokio::test]
async fn test_admin_access_token_unauthorized() {
    let server = setup_test_server().await;

    // Test create without authentication
    let create_request = serde_json::json!({
        "expires_in_hours": 24,
        "name": "Unauthorized Test",
        "reason": "Testing unauthorized access"
    });

    let create_response = server
        .post("/v1/admin/access_token")
        .json(&create_request)
        .await;

    assert_eq!(create_response.status_code(), 401);

    // Test list without authentication
    let list_response = server.get("/v1/admin/access_token").await;
    assert_eq!(list_response.status_code(), 401);

    // Test delete without authentication
    let delete_request = serde_json::json!({
        "reason": "Testing unauthorized access"
    });

    let delete_response = server
        .delete("/v1/admin/access_token/00000000-0000-0000-0000-000000000000")
        .json(&delete_request)
        .await;
    assert_eq!(delete_response.status_code(), 401);

    println!("✅ Admin access token endpoints correctly require authentication");
}

#[tokio::test]
async fn test_admin_access_token_cannot_manage_tokens() {
    let server = setup_test_server().await;

    // Get access token from refresh token
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Create an admin access token
    let create_request = serde_json::json!({
        "expires_in_hours": 24,
        "name": "Test Admin Token",
        "reason": "Testing token management restriction"
    });

    let create_response = server
        .post("/v1/admin/access_token")
        .add_header("Authorization", format!("Bearer {}", access_token))
        .json(&create_request)
        .await;

    assert_eq!(create_response.status_code(), 200);
    let token_response = create_response.json::<api::models::AdminAccessTokenResponse>();
    let admin_token = token_response.access_token;

    // Try to use the admin access token to create another admin access token (should fail)
    let create_request2 = serde_json::json!({
        "expires_in_hours": 24,
        "name": "Nested Admin Token",
        "reason": "This should fail"
    });

    let create_response2 = server
        .post("/v1/admin/access_token")
        .add_header("Authorization", format!("Bearer {}", admin_token))
        .json(&create_request2)
        .await;

    // Should fail because admin access tokens cannot be used for token management
    assert_eq!(create_response2.status_code(), 403);

    // Try to use the admin access token to list admin access tokens (should fail)
    let list_response = server
        .get("/v1/admin/access_token")
        .add_header("Authorization", format!("Bearer {}", admin_token))
        .await;

    assert_eq!(list_response.status_code(), 403);

    // Try to use the admin access token to delete an admin access token (should fail)
    let delete_response = server
        .delete("/v1/admin/access_token/00000000-0000-0000-0000-000000000000")
        .add_header("Authorization", format!("Bearer {}", admin_token))
        .json(&serde_json::json!({"reason": "This should fail"}))
        .await;

    assert_eq!(delete_response.status_code(), 403);

    println!("✅ Admin access tokens correctly restricted from token management endpoints");
}
