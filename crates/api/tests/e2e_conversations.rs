// Import common test utilities
mod common;

use common::*;

use api::models::{
    ConversationContentPart, ConversationItem, ResponseOutputContent, ResponseOutputItem,
};

// Helper functions for conversation and response tests
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

// ============================================
// Response Tests
// ============================================

/// When inference fails at the start (e.g. model not found), the stream emits response.failed
/// and the response has status=Failed with one failed output item (content empty).
#[tokio::test]
async fn test_response_stream_fails_with_failed_event_when_inference_fails_at_start() {
    let (server, _pool, _mock, database, _guard) = setup_test_server_with_pool().await;
    // Do NOT call setup_qwen_model so that the requested model is not in the DB
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let conversation = create_conversation(&server, api_key.clone()).await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": { "id": conversation.id },
            "input": "hello",
            "temperature": 0.7,
            "max_output_tokens": 10,
            "stream": true,
            "model": "non-existent-model-for-failed-test"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "POST /v1/responses should return 200"
    );
    let response_text = response.text();

    let mut saw_created = false;
    let mut saw_in_progress = false;
    let mut saw_failed = false;
    let mut saw_completed = false;
    let mut failed_text: Option<String> = None;

    for line_chunk in response_text.split("\n\n") {
        if line_chunk.trim().is_empty() {
            continue;
        }
        let mut event_type = "";
        let mut event_data = "";
        for line in line_chunk.lines() {
            if let Some(name) = line.strip_prefix("event: ") {
                event_type = name;
            } else if let Some(data) = line.strip_prefix("data: ") {
                event_data = data;
            }
        }
        if event_type == "response.created" {
            saw_created = true;
        } else if event_type == "response.in_progress" {
            saw_in_progress = true;
        } else if event_type == "response.failed" {
            saw_failed = true;
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(event_data) {
                failed_text = json.get("text").and_then(|v| v.as_str()).map(String::from);
            }
        } else if event_type == "response.completed" {
            saw_completed = true;
        }
    }

    assert!(saw_created, "Stream should contain response.created");
    assert!(
        saw_in_progress,
        "Stream should contain response.in_progress"
    );
    assert!(
        saw_failed,
        "Stream should contain response.failed when inference fails at start"
    );
    assert!(
        !saw_completed,
        "Stream should NOT contain response.completed when inference fails at start"
    );
    assert!(
        failed_text.as_deref().is_some_and(|s| !s.is_empty()),
        "response.failed event should have non-empty text (error message)"
    );

    // Verify DB: latest response for this conversation has status=Failed and one assistant item with status=failed, content=[]
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    let pool = database.pool();
    let client = pool.get().await.expect("db connection");
    let conv_uuid = uuid::Uuid::parse_str(
        conversation
            .id
            .strip_prefix("conv_")
            .unwrap_or(&conversation.id),
    )
    .expect("conv id");
    let resp_row = client
        .query_one(
            "SELECT id, status FROM responses WHERE conversation_id = $1 ORDER BY created_at DESC LIMIT 1",
            &[&conv_uuid],
        )
        .await
        .expect("query responses");
    let status: String = resp_row.get("status");
    assert_eq!(status, "failed", "Response status in DB should be failed");

    let item_rows = client
        .query(
            "SELECT item FROM response_items WHERE conversation_id = $1 ORDER BY created_at ASC",
            &[&conv_uuid],
        )
        .await
        .expect("query response_items");
    let assistant_items: Vec<serde_json::Value> = item_rows
        .into_iter()
        .filter_map(|row| {
            let item: serde_json::Value = row.get("item");
            if item.get("role").and_then(|v| v.as_str()) == Some("assistant") {
                Some(item)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        assistant_items.len(),
        1,
        "Should have exactly one assistant output item (the failed one)"
    );
    let item = &assistant_items[0];
    assert_eq!(item.get("status").and_then(|v| v.as_str()), Some("failed"));
    let content = item
        .get("content")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(content.is_empty(), "Failed item content should be empty");
}

#[tokio::test]
async fn test_responses_api() {
    let (server, _guard) = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

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
        _ => panic!("Expected Message item type"),
    }
}

#[tokio::test]
async fn test_streaming_responses_api() {
    let (server, _guard) = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

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
    assert!(streaming_response.output.iter().any(|item| {
        if let ResponseOutputItem::Message { content, .. } = item {
            content.iter().any(|part| {
                if let ResponseOutputContent::OutputText { text, .. } = part {
                    !text.is_empty()
                } else {
                    false
                }
            })
        } else {
            false
        }
    }));
}

// ============================================
// Usage Limit Enforcement Tests
// ============================================

#[tokio::test]
async fn test_responses_api_usage_limit_enforcement() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 1).await; // 1 nano-dollar (minimal)
    println!("Created organization: {org:?}");
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model_name = setup_qwen_model(&server).await;

    // Create a conversation for the response
    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "name": "Test Conversation",
            "description": "A test conversation"
        }))
        .await;

    assert_eq!(
        conversation_response.status_code(),
        201,
        "Failed to create conversation"
    );
    let conversation = conversation_response.json::<api::models::ConversationObject>();

    // First request should succeed (no usage yet)
    let response1 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation.id,
            },
            "input": "Hi",
            "model": model_name,
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    println!("First request status: {}", response1.status_code());
    // This might succeed or fail depending on timing

    // Wait for usage to be recorded
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Second request should fail with payment required
    let response2 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation.id,
            },
            "input": "Hi again",
            "model": model_name,
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    println!("Second request status: {}", response2.status_code());
    println!("Second request body: {}", response2.text());

    // Should get 402 Payment Required after exceeding limit
    assert!(
        response2.status_code() == 402,
        "Expected 402 Payment Required, got: {}",
        response2.status_code()
    );

    // Since we got 402, verify the error message
    let error_response = response2.json::<api::models::ErrorResponse>();
    assert!(
        error_response
            .error
            .message
            .contains("Credit limit exceeded."),
        "Error response should indicate no credits, got: {}",
        error_response.error.message
    );
}

#[tokio::test]
async fn test_responses_api_no_credits() {
    let (server, _guard) = setup_test_server().await;
    // Create org without credits (no limit set)
    let org = create_org(&server).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model_name = setup_qwen_model(&server).await;

    // Create a conversation for the response
    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "name": "Test Conversation",
            "description": "A test conversation"
        }))
        .await;

    assert_eq!(
        conversation_response.status_code(),
        201,
        "Failed to create conversation"
    );
    let conversation = conversation_response.json::<api::models::ConversationObject>();

    // Request should fail with payment required (no credits)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation.id,
            },
            "input": "Hi",
            "model": model_name,
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    assert_eq!(
        response.status_code(),
        402,
        "Expected 402 Payment Required when no credits are available"
    );

    let error_response = response.json::<api::models::ErrorResponse>();
    assert!(
        error_response
            .error
            .message
            .contains("No spending limit configured"),
        "Error response should indicate no credits, got: {}",
        error_response.error.message
    );
}

#[tokio::test]
async fn test_responses_api_zero_credits() {
    let (server, _guard) = setup_test_server().await;
    // Create org with zero credits
    let org = setup_org_with_credits(&server, 0).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model_name = setup_qwen_model(&server).await;

    // Create a conversation for the response
    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "name": "Test Conversation",
            "description": "A test conversation"
        }))
        .await;

    assert_eq!(
        conversation_response.status_code(),
        201,
        "Failed to create conversation"
    );
    let conversation = conversation_response.json::<api::models::ConversationObject>();

    // Request should fail with payment required (zero credits)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation.id,
            },
            "input": "Hi",
            "model": model_name,
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    assert_eq!(
        response.status_code(),
        402,
        "Expected 402 Payment Required when credits are zero"
    );

    let error_response = response.json::<api::models::ErrorResponse>();
    assert!(
        error_response
            .error
            .message
            .contains("Credit limit exceeded."),
        "Error response should indicate no credits, got: {}",
        error_response.error.message
    );
}

#[tokio::test]
async fn test_responses_api_sufficient_credits() {
    let (server, _guard) = setup_test_server().await;
    // Create org with sufficient credits
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model_name = setup_qwen_model(&server).await;

    // Create a conversation for the response
    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "name": "Test Conversation",
            "description": "A test conversation"
        }))
        .await;

    assert_eq!(
        conversation_response.status_code(),
        201,
        "Failed to create conversation"
    );
    let conversation = conversation_response.json::<api::models::ConversationObject>();

    // Request should succeed with sufficient credits
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation.id,
            },
            "input": "Say hello in exactly 5 words.",
            "model": model_name,
            "stream": false,
            "max_output_tokens": 50
        }))
        .await;

    println!("Response status: {}", response.status_code());
    assert_eq!(
        response.status_code(),
        200,
        "Expected 200 OK when sufficient credits are available"
    );

    let response_obj = response.json::<api::models::ResponseObject>();
    assert_eq!(
        response_obj.status,
        api::models::ResponseStatus::Completed,
        "Response should complete successfully"
    );
}

#[tokio::test]
async fn test_responses_api_streaming_usage_limit() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 1).await; // 1 nano-dollar (minimal)
    println!("Created organization: {org:?}");
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model_name = setup_qwen_model(&server).await;

    // Create a conversation for the response
    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "name": "Test Conversation",
            "description": "A test conversation"
        }))
        .await;

    assert_eq!(
        conversation_response.status_code(),
        201,
        "Failed to create conversation"
    );
    let conversation = conversation_response.json::<api::models::ConversationObject>();

    // First streaming request should succeed (no usage yet)
    let response1 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation.id,
            },
            "input": "Hi",
            "model": model_name,
            "stream": true,
            "max_output_tokens": 10
        }))
        .await;

    println!(
        "First streaming request status: {}",
        response1.status_code()
    );
    // This might succeed or fail depending on timing

    // Wait for usage to be recorded
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Second streaming request should fail with payment required
    let response2 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation.id,
            },
            "input": "Hi again",
            "model": model_name,
            "stream": true,
            "max_output_tokens": 10
        }))
        .await;

    println!(
        "Second streaming request status: {}",
        response2.status_code()
    );
    println!("Second streaming request body: {}", response2.text());

    // Should get 402 Payment Required after exceeding limit
    assert!(
        response2.status_code() == 402,
        "Expected 402 Payment Required, got: {}",
        response2.status_code()
    );

    // If we got 402, verify the error message
    let error_response = response2.json::<api::models::ErrorResponse>();
    assert!(
        error_response
            .error
            .message
            .contains("Credit limit exceeded."),
        "Error response should indicate no credits, got: {}",
        error_response.error.message
    );
}

// ============================================
// Conversation Tests
// ============================================

#[tokio::test]
async fn test_conversations_api() {
    let (server, _guard) = setup_test_server().await;
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
async fn test_create_conversation_items_backfill() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Test 1: Create a single item with simple text content
    let create_items_response = server
        .post(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "items": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "Hello!"}
                    ]
                }
            ]
        }))
        .await;

    assert_eq!(create_items_response.status_code(), 200);
    let items_response = create_items_response.json::<api::models::ConversationItemList>();
    assert_eq!(items_response.data.len(), 1);
    assert_eq!(items_response.object, "list");
    assert!(!items_response.first_id.is_empty());
    assert!(!items_response.last_id.is_empty());
    assert!(!items_response.has_more);

    // Verify the item content
    match &items_response.data[0] {
        ConversationItem::Message {
            role,
            content,
            status,
            ..
        } => {
            assert_eq!(role, "user");
            assert!(matches!(status, api::models::ResponseItemStatus::Completed));
            assert_eq!(content.len(), 1);
            match &content[0] {
                ConversationContentPart::InputText { text } => {
                    assert_eq!(text, "Hello!");
                }
                _ => panic!("Expected InputText content part"),
            }
        }
        _ => panic!("Expected Message item type"),
    }

    // Test 2: Verify items can be retrieved via list endpoint
    let list_response =
        list_conversation_items(&server, conversation.id.clone(), api_key.clone()).await;
    assert!(
        !list_response.data.is_empty(),
        "Should have at least the backfilled item"
    );

    // Find our backfilled item
    let backfilled_item = list_response.data.iter().find(|item| match item {
        ConversationItem::Message { content, .. } => content.iter().any(
            |part| matches!(part, ConversationContentPart::InputText { text } if text == "Hello!"),
        ),
        _ => false,
    });
    assert!(
        backfilled_item.is_some(),
        "Backfilled item should be retrievable"
    );

    println!("✅ Basic backfill test passed");
}

#[tokio::test]
async fn test_create_conversation_items_multiple() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Test: Create multiple items (up to 20)
    let create_items_response = server
        .post(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "items": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "First message"}
                    ]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "Second message"}
                    ]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "input_text", "text": "Assistant response"}
                    ]
                }
            ]
        }))
        .await;

    assert_eq!(create_items_response.status_code(), 200);
    let items_response = create_items_response.json::<api::models::ConversationItemList>();
    assert_eq!(items_response.data.len(), 3);
    assert_eq!(items_response.first_id, items_response.data[0].id());
    assert_eq!(items_response.last_id, items_response.data[2].id());

    // Verify all items were created correctly
    assert_eq!(items_response.data[0].role(), "user");
    assert_eq!(items_response.data[1].role(), "user");
    assert_eq!(items_response.data[2].role(), "assistant");

    println!("✅ Multiple items backfill test passed");
}

#[tokio::test]
async fn test_create_conversation_items_validation_empty() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Test: Empty items array should fail
    let create_items_response = server
        .post(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "items": []
        }))
        .await;

    assert_eq!(create_items_response.status_code(), 400);
    let error_response = create_items_response.json::<api::models::ErrorResponse>();
    assert!(
        error_response.error.message.contains("empty")
            || error_response.error.message.contains("Items")
    );

    println!("✅ Empty items validation test passed");
}

#[tokio::test]
async fn test_create_conversation_items_validation_too_many() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Test: More than 20 items should fail
    let items: Vec<serde_json::Value> = (0..21)
        .map(|i| {
            serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": format!("Message {}", i)}
                ]
            })
        })
        .collect();

    let create_items_response = server
        .post(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "items": items
        }))
        .await;

    assert_eq!(create_items_response.status_code(), 400);
    let error_response = create_items_response.json::<api::models::ErrorResponse>();
    assert!(
        error_response.error.message.contains("20")
            || error_response.error.message.contains("more than")
    );

    println!("✅ Too many items validation test passed");
}

#[tokio::test]
async fn test_create_conversation_items_nonexistent_conversation() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Test: Non-existent conversation should fail
    let fake_conv_id = "conv_00000000-0000-0000-0000-000000000000";
    let create_items_response = server
        .post(format!("/v1/conversations/{fake_conv_id}/items").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "items": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "Hello!"}
                    ]
                }
            ]
        }))
        .await;

    assert_eq!(create_items_response.status_code(), 400);
    let error_response = create_items_response.json::<api::models::ErrorResponse>();
    assert!(
        error_response.error.message.contains("not found")
            || error_response.error.message.contains("Conversation"),
        "Error message should indicate conversation not found"
    );

    println!("✅ Non-existent conversation validation test passed");
}

#[tokio::test]
async fn test_create_conversation_items_max_limit() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Test: Exactly 20 items should succeed (max limit)
    let items: Vec<serde_json::Value> = (0..20)
        .map(|i| {
            serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": format!("Message {}", i)}
                ]
            })
        })
        .collect();

    let create_items_response = server
        .post(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "items": items
        }))
        .await;

    assert_eq!(create_items_response.status_code(), 200);
    let items_response = create_items_response.json::<api::models::ConversationItemList>();
    assert_eq!(items_response.data.len(), 20);

    println!("✅ Max limit (20 items) test passed");
}

#[tokio::test]
async fn test_create_conversation_items_text_content() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Test: Content as simple text string (not array)
    let create_items_response = server
        .post(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "items": [
                {
                    "type": "message",
                    "role": "user",
                    "content": "Simple text content"
                }
            ]
        }))
        .await;

    assert_eq!(create_items_response.status_code(), 200);
    let items_response = create_items_response.json::<api::models::ConversationItemList>();
    assert_eq!(items_response.data.len(), 1);

    match &items_response.data[0] {
        ConversationItem::Message { content, .. } => {
            assert_eq!(content.len(), 1);
            match &content[0] {
                ConversationContentPart::InputText { text } => {
                    assert_eq!(text, "Simple text content");
                }
                _ => panic!("Expected InputText content part"),
            }
        }
        _ => panic!("Expected Message item type"),
    }

    println!("✅ Text content format test passed");
}

#[tokio::test]
async fn test_create_conversation_items_with_file_content() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Test: Create item with file content
    let create_items_response = server
        .post(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "items": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "Could you read the file?"},
                        {"type": "input_file", "file_id": "32af7670-f5b9-47a0-a952-20d5d3831e67"},
                        {"type": "input_text", "text": "Discussion about [file-32af7670f5b947a0a95220d5d3831e67] in text"}
                    ]
                }
            ]
        }))
        .await;

    assert_eq!(create_items_response.status_code(), 200);
    let items_response = create_items_response.json::<api::models::ConversationItemList>();
    assert_eq!(items_response.data.len(), 1);

    // Verify the item content - this is the key test
    match &items_response.data[0] {
        ConversationItem::Message { role, content, .. } => {
            assert_eq!(role, "user");
            assert_eq!(content.len(), 3);

            // First content part should be text
            match &content[0] {
                ConversationContentPart::InputText { text } => {
                    assert_eq!(text, "Could you read the file?");
                }
                _ => panic!("Expected InputText content part"),
            }

            // Second content part should be file (valid UUID)
            match &content[1] {
                ConversationContentPart::InputFile { file_id, detail } => {
                    assert_eq!(file_id, "32af7670-f5b9-47a0-a952-20d5d3831e67");
                    assert_eq!(detail, &None);
                }
                _ => panic!("Expected InputFile content part but got: {:?}", &content[1]),
            }

            // Third content part should be text (invalid UUID format)
            match &content[2] {
                ConversationContentPart::InputText { text } => {
                    assert_eq!(
                        text,
                        "Discussion about [file-32af7670f5b947a0a95220d5d3831e67] in text"
                    );
                }
                _ => panic!(
                    "Expected InputText for invalid file ID but got: {:?}",
                    &content[2]
                ),
            }
        }
        _ => panic!("Expected Message item type"),
    }

    // Verify items can be retrieved via list endpoint
    let list_response =
        list_conversation_items(&server, conversation.id.clone(), api_key.clone()).await;

    // Find our backfilled item with file content
    let file_item = list_response.data.iter().find(|item| match item {
        ConversationItem::Message { content, .. } => content
            .iter()
            .any(|part| matches!(part, ConversationContentPart::InputFile { .. })),
        _ => false,
    });
    assert!(
        file_item.is_some(),
        "Backfilled item with file should be retrievable"
    );

    println!("✅ File content parsing test passed");
}

#[tokio::test]
async fn test_create_conversation_items_different_roles() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Test: Different roles (user, assistant, system)
    let create_items_response = server
        .post(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "items": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "User message"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "input_text", "text": "Assistant message"}]
                },
                {
                    "type": "message",
                    "role": "system",
                    "content": [{"type": "input_text", "text": "System message"}]
                }
            ]
        }))
        .await;

    assert_eq!(create_items_response.status_code(), 200);
    let items_response = create_items_response.json::<api::models::ConversationItemList>();
    assert_eq!(items_response.data.len(), 3);

    // Verify roles
    assert_eq!(items_response.data[0].role(), "user");
    assert_eq!(items_response.data[1].role(), "assistant");
    assert_eq!(items_response.data[2].role(), "system");

    println!("✅ Different roles test passed");
}

#[tokio::test]
async fn test_conversation_items_pagination() {
    let (server, _guard) = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let models = list_models(&server, api_key.clone()).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Create 5 responses to generate multiple items (each response creates 2 items: user input + assistant output)
    let max_tokens = 10;
    for i in 0..5 {
        let message = format!("Test message {}", i + 1);
        create_response(
            &server,
            conversation.id.clone(),
            models.data[0].id.clone(),
            message,
            max_tokens,
            api_key.clone(),
        )
        .await;
    }

    // Test 1: Fetch first page with limit of 3
    let response = server
        .get(format!("/v1/conversations/{}/items?limit=3", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(response.status_code(), 200);
    let first_page = response.json::<api::models::ConversationItemList>();

    // Should have exactly 3 items
    assert_eq!(first_page.data.len(), 3, "First page should have 3 items");

    // Should indicate there are more items
    assert!(first_page.has_more, "Should indicate more items exist");

    // Should have first_id and last_id
    assert!(!first_page.first_id.is_empty(), "Should have first_id");
    assert!(!first_page.last_id.is_empty(), "Should have last_id");

    // Test 2: Fetch next page using 'after' cursor
    let last_id_from_page1 = first_page.last_id.clone();
    let response2 = server
        .get(
            format!(
                "/v1/conversations/{}/items?limit=3&after={}",
                conversation.id, last_id_from_page1
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(response2.status_code(), 200);
    let second_page = response2.json::<api::models::ConversationItemList>();

    // Should have exactly 3 items
    assert_eq!(second_page.data.len(), 3, "Second page should have 3 items");

    // Should indicate there are more items
    assert!(second_page.has_more, "Should indicate more items exist");

    // Test 3: Fetch third page
    let last_id_from_page2 = second_page.last_id.clone();
    let response3 = server
        .get(
            format!(
                "/v1/conversations/{}/items?limit=3&after={}",
                conversation.id, last_id_from_page2
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(response3.status_code(), 200);
    let third_page = response3.json::<api::models::ConversationItemList>();

    // Should have 3 items (limited by the limit parameter)
    assert_eq!(third_page.data.len(), 3, "Third page should have 3 items");

    // Should indicate there is 1 more item
    assert!(third_page.has_more, "Should indicate more items exist");

    // Test 4: Fetch fourth (final) page
    let last_id_from_page3 = third_page.last_id.clone();
    let response4 = server
        .get(
            format!(
                "/v1/conversations/{}/items?limit=3&after={}",
                conversation.id, last_id_from_page3
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(response4.status_code(), 200);
    let fourth_page = response4.json::<api::models::ConversationItemList>();

    // Should have 1 item (the last one)
    assert_eq!(fourth_page.data.len(), 1, "Fourth page should have 1 item");

    // Should NOT indicate there are more items
    assert!(
        !fourth_page.has_more,
        "Should NOT indicate more items exist"
    );

    // Test 5: Verify no duplicate items across pages
    let all_ids: Vec<String> = first_page
        .data
        .iter()
        .chain(second_page.data.iter())
        .chain(third_page.data.iter())
        .chain(fourth_page.data.iter())
        .map(|item| match item {
            ConversationItem::Message { id, .. } => id.clone(),
            ConversationItem::ToolCall { id, .. } => id.clone(),
            ConversationItem::WebSearchCall { id, .. } => id.clone(),
            ConversationItem::Reasoning { id, .. } => id.clone(),
            ConversationItem::McpListTools { id, .. } => id.clone(),
            ConversationItem::McpCall { id, .. } => id.clone(),
            ConversationItem::McpApprovalRequest { id, .. } => id.clone(),
        })
        .collect();

    // Check for uniqueness
    let unique_ids: std::collections::HashSet<_> = all_ids.iter().collect();
    assert_eq!(
        all_ids.len(),
        unique_ids.len(),
        "All items should be unique across pages"
    );

    // Test 6: Fetch all items without pagination (default limit of 100)
    let response_all = server
        .get(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(response_all.status_code(), 200);
    let all_items = response_all.json::<api::models::ConversationItemList>();

    // Should have all 10 items
    assert_eq!(
        all_items.data.len(),
        10,
        "Should fetch all 10 items with default limit"
    );

    // Should NOT indicate there are more items
    assert!(
        !all_items.has_more,
        "Should NOT indicate more items with all items fetched"
    );

    println!("✅ Conversation items pagination working correctly");
}

#[tokio::test]
async fn test_response_previous_next_relationships() {
    use common::mock_prompts;
    use inference_providers::mock::{RequestMatcher, ResponseTemplate};

    // Use setup_test_server_with_pool to get access to mock provider
    let (server, _pool, mock, _db, _guard) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;
    println!("Created conversation: {}", conversation.id);

    // Set up mock expectations for each request in the conversation tree
    // Parent request: "What is the capital of France?"
    let parent_prompt = mock_prompts::build_prompt("What is the capital of France?");
    mock.when(RequestMatcher::ExactPrompt(parent_prompt))
        .respond_with(ResponseTemplate::new("Paris is the capital of France."))
        .await;

    // Create first response (parent)
    let parent_response = create_response(
        &server,
        conversation.id.clone(),
        "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
        "What is the capital of France?".to_string(),
        100,
        api_key.clone(),
    )
    .await;

    println!("Created parent response: {}", parent_response.id);

    // Verify parent response has no next responses initially
    assert!(
        parent_response.next_response_ids.is_empty(),
        "Parent response should have no next responses initially"
    );
    assert!(
        parent_response.previous_response_id.is_none(),
        "Parent response should have no previous_response_id"
    );

    // Branch 1: First follow-up from parent
    // Expected context: parent user + parent assistant + new user message
    let response1_prompt = mock_prompts::build_prompt(
        "What is the capital of France? Paris is the capital of France. Tell me more about that.",
    );
    mock.when(RequestMatcher::ExactPrompt(response1_prompt))
        .respond_with(ResponseTemplate::new(
            "Paris is known for the Eiffel Tower and rich history.",
        ))
        .await;

    // Create first follow-up response (with conversation + previous_response_id for context filtering)
    let response1 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation.id
            },
            "input": "Tell me more about that.",
            "temperature": 0.7,
            "max_output_tokens": 100,
            "stream": false,
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "previous_response_id": parent_response.id
        }))
        .await;

    assert_eq!(response1.status_code(), 200);
    let response1 = response1.json::<api::models::ResponseObject>();
    println!("Created response1: {}", response1.id);

    // Verify response1 has parent reference
    assert_eq!(
        response1.previous_response_id,
        Some(parent_response.id.clone()),
        "Response1 should reference parent as previous_response_id"
    );
    assert!(
        response1.next_response_ids.is_empty(),
        "Response1 should have no next responses initially"
    );

    // Branch 2: Second follow-up from parent (creates a sibling branch)
    // Expected context: parent user + parent assistant + new user message
    // This should be the SAME context as response1 (both branch from parent)
    let response2_prompt = mock_prompts::build_prompt(
        "What is the capital of France? Paris is the capital of France. What about its history?",
    );
    mock.when(RequestMatcher::ExactPrompt(response2_prompt))
        .respond_with(ResponseTemplate::new(
            "Paris has a long history dating back to Roman times.",
        ))
        .await;

    // Create second follow-up response from the same parent (with conversation + previous_response_id)
    let response2 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation.id
            },
            "input": "What about its history?",
            "temperature": 0.7,
            "max_output_tokens": 100,
            "stream": false,
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "previous_response_id": parent_response.id
        }))
        .await;

    assert_eq!(response2.status_code(), 200);
    let response2 = response2.json::<api::models::ResponseObject>();
    println!("Created response2: {}", response2.id);

    // Verify response2 has parent reference
    assert_eq!(
        response2.previous_response_id,
        Some(parent_response.id.clone()),
        "Response2 should reference parent as previous_response_id"
    );

    // CRITICAL TEST: Nested response following branch 1
    // This verifies that context filtering works correctly
    // Expected context: parent → response1 path ONLY (should NOT include response2)
    // Format: parent_user + parent_assistant + response1_user + response1_assistant + nested_user
    let nested_prompt = mock_prompts::build_prompt(
        "What is the capital of France? Paris is the capital of France. Tell me more about that. Paris is known for the Eiffel Tower and rich history. Can you elaborate?"
    );
    mock.when(RequestMatcher::ExactPrompt(nested_prompt))
        .respond_with(ResponseTemplate::new(
            "The Eiffel Tower was built in 1889 for the World's Fair.",
        ))
        .await;

    // Create nested response (follows response1, with conversation + previous_response_id for filtering)
    let nested_response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation.id
            },
            "input": "Can you elaborate?",
            "temperature": 0.7,
            "max_output_tokens": 100,
            "stream": false,
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "previous_response_id": response1.id
        }))
        .await;

    assert_eq!(
        nested_response.status_code(),
        200,
        "Nested response should succeed"
    );
    let nested_response = nested_response.json::<api::models::ResponseObject>();
    println!("Created nested response: {}", nested_response.id);

    // Verify nested response has response1 as previous
    assert_eq!(
        nested_response.previous_response_id,
        Some(response1.id.clone()),
        "Nested response should reference response1 as previous_response_id"
    );

    // Verify the response content to ensure the mock was matched
    // This confirms that the correct context was passed (parent → response1 path, excluding response2)
    let has_expected_content = nested_response.output.iter().any(|item| {
        if let api::models::ResponseOutputItem::Message { content, .. } = item {
            content.iter().any(|part| {
                if let api::models::ResponseOutputContent::OutputText { text, .. } = part {
                    text.contains("Eiffel Tower was built in 1889")
                } else {
                    false
                }
            })
        } else {
            false
        }
    });
    assert!(
        has_expected_content,
        "Nested response should contain expected content from the mock, confirming correct context filtering"
    );

    println!("✅ Response previous-next relationships and context filtering working correctly");
    println!("   - Parent: {}", parent_response.id);
    println!(
        "   - Response1: {} (previous: {})",
        response1.id,
        response1.previous_response_id.as_ref().unwrap()
    );
    println!(
        "   - Response2: {} (previous: {})",
        response2.id,
        response2.previous_response_id.as_ref().unwrap()
    );
    println!(
        "   - Nested: {} (previous: {}) - context filtered to parent→response1 path only",
        nested_response.id,
        nested_response.previous_response_id.as_ref().unwrap()
    );
}

#[tokio::test]
async fn test_first_turn_items_have_root_response_parent() {
    let (server, _guard) = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;
    println!("Created conversation: {}", conversation.id);

    // Create the first response in this conversation (no previous_response_id)
    let first_response = create_response(
        &server,
        conversation.id.clone(),
        "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
        "Hello, world!".to_string(),
        100,
        api_key.clone(),
    )
    .await;

    println!("Created first response: {}", first_response.id);

    // List conversation items and verify that all items belonging to the first
    // response share a non-empty previous_response_id that is different from
    // the response_id itself (i.e., they point to the hidden root_response).
    let items_list =
        list_conversation_items(&server, conversation.id.clone(), api_key.clone()).await;
    assert!(
        !items_list.data.is_empty(),
        "Conversation should contain at least the first turn items"
    );

    let mut parent_ids: Vec<String> = Vec::new();

    for item in &items_list.data {
        match item {
            api::models::ConversationItem::Message {
                response_id,
                previous_response_id,
                ..
            }
            | api::models::ConversationItem::Reasoning {
                response_id,
                previous_response_id,
                ..
            } => {
                if response_id == &first_response.id {
                    let prev = previous_response_id
                        .as_ref()
                        .unwrap_or_else(|| panic!(
                            "First-turn item {} should have a previous_response_id (root_response parent)",
                            response_id
                        ));
                    parent_ids.push(prev.clone());
                }
            }
            _ => {}
        }
    }

    assert!(
        !parent_ids.is_empty(),
        "Expected at least one item belonging to the first response"
    );

    // All parent IDs should be the same and different from the first response ID.
    let root_id = &parent_ids[0];
    for pid in &parent_ids {
        assert_eq!(
            pid, root_id,
            "All first-turn items should share the same root_response parent"
        );
    }
    assert_ne!(
        root_id, &first_response.id,
        "root_response parent ID should be different from the first response ID"
    );
}

#[tokio::test]
async fn test_first_turn_regenerate_creates_siblings_under_root_response() {
    use std::collections::HashSet;

    let (server, _guard) = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation and the first response (no previous_response_id)
    let conversation = create_conversation(&server, api_key.clone()).await;
    let first_response = create_response(
        &server,
        conversation.id.clone(),
        "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
        "Hello, root!".to_string(),
        100,
        api_key.clone(),
    )
    .await;

    // Fetch items and extract the root_response ID from one of the first-turn items
    let items_list =
        list_conversation_items(&server, conversation.id.clone(), api_key.clone()).await;
    let root_response_id = items_list
        .data
        .iter()
        .find_map(|item| match item {
            api::models::ConversationItem::Message {
                response_id,
                previous_response_id,
                ..
            }
            | api::models::ConversationItem::Reasoning {
                response_id,
                previous_response_id,
                ..
            } if response_id == &first_response.id => previous_response_id.clone(),
            _ => None,
        })
        .expect("Expected first-turn items to have a root_response previous_response_id");

    println!(
        "First response {} has root_response parent {}",
        first_response.id, root_response_id
    );

    // Create a second first-turn response by using the root_response ID as previous_response_id.
    // This simulates "regenerate" of the first turn: both responses share the same root parent.
    let regen_response_http = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "Hello again!",
            "temperature": 0.7,
            "max_output_tokens": 100,
            "stream": false,
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "previous_response_id": root_response_id
        }))
        .await;

    assert_eq!(regen_response_http.status_code(), 200);
    let regen_response = regen_response_http.json::<api::models::ResponseObject>();
    println!(
        "Created regenerated first-turn response: {}",
        regen_response.id
    );

    // List items again and collect all distinct response_ids that share the same
    // root_response parent. We expect at least the original first_response and
    // the regenerated response.
    let items_after =
        list_conversation_items(&server, conversation.id.clone(), api_key.clone()).await;
    let mut first_turn_response_ids: HashSet<String> = HashSet::new();

    for item in &items_after.data {
        match item {
            api::models::ConversationItem::Message {
                response_id,
                previous_response_id,
                ..
            }
            | api::models::ConversationItem::Reasoning {
                response_id,
                previous_response_id,
                ..
            } => {
                if previous_response_id.as_deref() == Some(&root_response_id) {
                    first_turn_response_ids.insert(response_id.clone());
                }
            }
            _ => {}
        }
    }

    assert!(
        first_turn_response_ids.contains(&first_response.id),
        "Original first response should be a child of root_response"
    );
    assert!(
        first_turn_response_ids.contains(&regen_response.id),
        "Regenerated first-turn response should also be a child of root_response"
    );
    assert!(
        first_turn_response_ids.len() >= 2,
        "Expected at least two distinct first-turn responses under the same root_response"
    );
}

#[tokio::test]
async fn test_response_previous_next_relationships_streaming() {
    let (server, _guard) = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;
    println!("Created conversation: {}", conversation.id);

    // Create first response (parent) with streaming
    let (_, parent_response) = create_response_stream(
        &server,
        conversation.id.clone(),
        "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
        "What is the capital of France?".to_string(),
        100,
        api_key.clone(),
    )
    .await;

    println!(
        "Created parent response (streaming): {}",
        parent_response.id
    );

    // Verify parent response has no next responses initially
    assert!(
        parent_response.next_response_ids.is_empty(),
        "Parent response should have no next responses initially"
    );

    // Create follow-up response with streaming (conversation inherited from parent)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "Tell me more about that.",
            "temperature": 0.7,
            "max_output_tokens": 100,
            "stream": true,
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "previous_response_id": parent_response.id
        }))
        .await;

    if response.status_code() != 200 {
        println!("Error response: {}", response.text());
    }
    assert_eq!(response.status_code(), 200);

    // Parse streaming response
    let response_text = response.text();
    let mut next_response: Option<api::models::ResponseObject> = None;

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

        if !event_data.is_empty() && event_type == "response.completed" {
            if let Ok(event_json) = serde_json::from_str::<serde_json::Value>(event_data) {
                if let Some(response_obj) = event_json.get("response") {
                    next_response = Some(
                        serde_json::from_value::<api::models::ResponseObject>(response_obj.clone())
                            .expect("Failed to parse response.completed event"),
                    );
                }
            }
        }
    }

    let follow_up_response = next_response.expect("Should have received completed response");
    println!(
        "Created follow-up response (streaming): {}",
        follow_up_response.id
    );

    // Verify follow-up has parent reference
    assert_eq!(
        follow_up_response.previous_response_id,
        Some(parent_response.id.clone()),
        "Follow-up should reference parent as previous_response_id"
    );
    assert!(
        follow_up_response.next_response_ids.is_empty(),
        "Follow-up should have no next responses initially"
    );

    println!("✅ Response previous-next relationships working correctly with streaming");
    println!("   - Parent: {}", parent_response.id);
    println!(
        "   - Follow-up: {} (previous: {})",
        follow_up_response.id,
        follow_up_response.previous_response_id.as_ref().unwrap()
    );
}

#[tokio::test]
async fn test_conversation_items_include_response_metadata() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Create parent response
    let parent_response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "What is Rust?",
            "temperature": 0.7,
            "max_output_tokens": 100,
            "stream": false,
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": conversation.id
        }))
        .await;

    assert_eq!(parent_response.status_code(), 200);
    let parent_response = parent_response.json::<api::models::ResponseObject>();
    println!("Created parent response: {}", parent_response.id);

    // Create follow-up response
    let follow_up_response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "Tell me more about that.",
            "temperature": 0.7,
            "max_output_tokens": 100,
            "stream": false,
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "previous_response_id": parent_response.id
        }))
        .await;

    assert_eq!(follow_up_response.status_code(), 200);
    let follow_up_response = follow_up_response.json::<api::models::ResponseObject>();
    println!("Created follow-up response: {}", follow_up_response.id);

    // List conversation items
    let items_response = server
        .get(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(items_response.status_code(), 200);
    let items_list = items_response.json::<api::models::ConversationItemList>();

    println!("Retrieved {} conversation items", items_list.data.len());

    // Verify each item has the new metadata fields
    for item in &items_list.data {
        match item {
            api::models::ConversationItem::Message {
                id,
                response_id,
                previous_response_id,
                next_response_ids,
                created_at,
                ..
            } => {
                println!("  Item {id}: response_id={response_id}, previous_response_id={previous_response_id:?}, next_response_ids={next_response_ids:?}, created_at={created_at}");

                // Verify required fields are populated
                assert!(!response_id.is_empty(), "response_id should not be empty");
                assert!(*created_at > 0, "created_at should be a valid timestamp");

                // If this item belongs to the follow-up response, verify it has the parent's ID
                if response_id == &follow_up_response.id {
                    assert_eq!(
                        previous_response_id.as_ref(),
                        Some(&parent_response.id),
                        "Follow-up response item should have parent's ID in previous_response_id"
                    );
                }
            }
            _ => {
                // For non-message items, just verify they have the metadata
                // (could add similar checks for ToolCall, WebSearchCall, Reasoning)
            }
        }
    }

    // Verify items are sorted by created_at (ascending order)
    let mut prev_timestamp = 0i64;
    for item in &items_list.data {
        let current_timestamp = match item {
            api::models::ConversationItem::Message { created_at, .. } => Some(*created_at),
            api::models::ConversationItem::ToolCall { created_at, .. } => Some(*created_at),
            api::models::ConversationItem::WebSearchCall { created_at, .. } => Some(*created_at),
            api::models::ConversationItem::Reasoning { created_at, .. } => Some(*created_at),
            // MCP items don't have created_at field in the API model
            api::models::ConversationItem::McpListTools { .. } => None,
            api::models::ConversationItem::McpCall { .. } => None,
            api::models::ConversationItem::McpApprovalRequest { .. } => None,
        };
        if let Some(ts) = current_timestamp {
            assert!(
                ts >= prev_timestamp,
                "Items should be sorted by created_at in ascending order"
            );
            prev_timestamp = ts;
        }
    }

    println!("✅ Conversation items include response metadata (response_id, previous_response_id, next_response_ids, created_at)");
    println!("✅ Items are sorted by created_at in ascending order");
}

// ============================================
// Conversation Management Tests (Pin, Archive, Clone, Rename, Delete)
// ============================================

#[tokio::test]
async fn test_pin_unpin_conversation() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;
    println!("Created conversation: {}", conversation.id);

    // Verify conversation is not pinned initially
    let get_response = server
        .get(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(get_response.status_code(), 200);
    let conv = get_response.json::<api::models::ConversationObject>();
    assert_eq!(conv.object, "conversation");

    // Test: Pin the conversation
    let now = chrono::Utc::now().timestamp();
    let pin_response = server
        .post(format!("/v1/conversations/{}/pin", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(pin_response.status_code(), 200);
    let pinned_conv = pin_response.json::<api::models::ConversationObject>();

    // Verify pinned_at is present and valid
    let pinned_at = pinned_conv
        .metadata
        .get("pinned_at")
        .expect("pinned_at should be present in metadata")
        .as_i64()
        .expect("pinned_at should be a number");
    assert!(
        pinned_at >= now,
        "pinned_at ({pinned_at}) should be >= now ({now})"
    );
    assert_eq!(pinned_conv.id, conversation.id);

    // Verify archived_at is not present when only pinned
    assert!(
        pinned_conv.metadata.get("archived_at").is_none(),
        "archived_at should not be present for pinned-only conversation"
    );
    println!("✅ Conversation pinned successfully with pinned_at timestamp");

    // Test: Unpin the conversation
    let unpin_response = server
        .delete(format!("/v1/conversations/{}/pin", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(unpin_response.status_code(), 200);
    let unpinned_conv = unpin_response.json::<api::models::ConversationObject>();
    assert_eq!(unpinned_conv.id, conversation.id);

    // Verify pinned_at is removed after unpinning
    assert!(
        unpinned_conv.metadata.get("pinned_at").is_none(),
        "pinned_at should not be present after unpinning"
    );
    println!("✅ Conversation unpinned successfully, pinned_at removed");

    // Test: Pinning again should be idempotent
    let now2 = chrono::Utc::now().timestamp();
    let pin_again_response = server
        .post(format!("/v1/conversations/{}/pin", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(pin_again_response.status_code(), 200);
    let repinned_conv = pin_again_response.json::<api::models::ConversationObject>();

    // Verify pinned_at is present again
    let repinned_at = repinned_conv
        .metadata
        .get("pinned_at")
        .expect("pinned_at should be present after re-pinning")
        .as_i64()
        .expect("pinned_at should be a number");
    assert!(
        repinned_at >= now2,
        "pinned_at should be updated to current time on re-pin"
    );
    println!("✅ Pin operation is idempotent and updates pinned_at timestamp");
}

#[tokio::test]
async fn test_archive_unarchive_conversation() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;
    println!("Created conversation: {}", conversation.id);

    // Test: Archive the conversation
    let now = chrono::Utc::now().timestamp();
    let archive_response = server
        .post(format!("/v1/conversations/{}/archive", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(archive_response.status_code(), 200);
    let archived_conv = archive_response.json::<api::models::ConversationObject>();
    assert_eq!(archived_conv.id, conversation.id);

    // Verify archived_at is present and valid
    let archived_at = archived_conv
        .metadata
        .get("archived_at")
        .expect("archived_at should be present in metadata")
        .as_i64()
        .expect("archived_at should be a number");
    assert!(
        archived_at >= now,
        "archived_at ({archived_at}) should be >= now ({now})"
    );

    // Verify pinned_at is not present when only archived
    assert!(
        archived_conv.metadata.get("pinned_at").is_none(),
        "pinned_at should not be present for archived-only conversation"
    );
    println!("✅ Conversation archived successfully with archived_at timestamp");

    // Test: Unarchive the conversation
    let unarchive_response = server
        .delete(format!("/v1/conversations/{}/archive", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(unarchive_response.status_code(), 200);
    let unarchived_conv = unarchive_response.json::<api::models::ConversationObject>();
    assert_eq!(unarchived_conv.id, conversation.id);

    // Verify archived_at is removed after unarchiving
    assert!(
        unarchived_conv.metadata.get("archived_at").is_none(),
        "archived_at should not be present after unarchiving"
    );
    println!("✅ Conversation unarchived successfully, archived_at removed");

    // Test: Archiving again should be idempotent
    let now2 = chrono::Utc::now().timestamp();
    let archive_again_response = server
        .post(format!("/v1/conversations/{}/archive", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(archive_again_response.status_code(), 200);
    let rearchived_conv = archive_again_response.json::<api::models::ConversationObject>();

    // Verify archived_at is present again
    let rearchived_at = rearchived_conv
        .metadata
        .get("archived_at")
        .expect("archived_at should be present after re-archiving")
        .as_i64()
        .expect("archived_at should be a number");
    assert!(
        rearchived_at >= now2,
        "archived_at should be updated to current time on re-archive"
    );
    println!("✅ Archive operation is idempotent and updates archived_at timestamp");
}

#[tokio::test]
async fn test_rename_conversation_via_metadata() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation with initial title in metadata
    let create_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": {
                "title": "Original Title",
                "description": "Test conversation"
            }
        }))
        .await;
    assert_eq!(create_response.status_code(), 201);
    let conversation = create_response.json::<api::models::ConversationObject>();
    println!("Created conversation: {}", conversation.id);

    // Verify initial metadata
    assert_eq!(
        conversation.metadata.get("title").and_then(|v| v.as_str()),
        Some("Original Title")
    );

    // Test: Update conversation name via metadata
    let update_response = server
        .post(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": {
                "title": "Updated Title",
                "description": "Updated description"
            }
        }))
        .await;
    assert_eq!(update_response.status_code(), 200);
    let updated_conv = update_response.json::<api::models::ConversationObject>();

    // Verify updated metadata
    assert_eq!(
        updated_conv.metadata.get("title").and_then(|v| v.as_str()),
        Some("Updated Title")
    );
    assert_eq!(
        updated_conv
            .metadata
            .get("description")
            .and_then(|v| v.as_str()),
        Some("Updated description")
    );
    println!("✅ Conversation renamed via metadata update");

    // Test: Get conversation to verify persistence
    let get_response = server
        .get(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(get_response.status_code(), 200);
    let fetched_conv = get_response.json::<api::models::ConversationObject>();
    assert_eq!(
        fetched_conv.metadata.get("title").and_then(|v| v.as_str()),
        Some("Updated Title")
    );
    println!("✅ Metadata changes persisted");
}

#[tokio::test]
async fn test_pin_rename_unpin_conversation() {
    // Test the bug: pin -> rename -> unpin should remove pinned_at from metadata
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let create_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": {
                "title": "Test Conversation"
            }
        }))
        .await;
    assert_eq!(create_response.status_code(), 201);
    let conversation = create_response.json::<api::models::ConversationObject>();
    println!("Created conversation: {}", conversation.id);

    // Step 1: Pin the conversation
    let pin_response = server
        .post(format!("/v1/conversations/{}/pin", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(pin_response.status_code(), 200);
    let pinned_conv = pin_response.json::<api::models::ConversationObject>();

    // Verify pinned_at is present
    assert!(
        pinned_conv.metadata.get("pinned_at").is_some(),
        "pinned_at should be present after pinning"
    );
    println!("✅ Conversation pinned successfully");

    // Step 2: Rename the conversation (update metadata)
    let rename_response = server
        .post(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": {
                "title": "Renamed Conversation"
            }
        }))
        .await;
    assert_eq!(rename_response.status_code(), 200);
    let renamed_conv = rename_response.json::<api::models::ConversationObject>();

    // Verify title was updated
    assert_eq!(
        renamed_conv.metadata.get("title").and_then(|v| v.as_str()),
        Some("Renamed Conversation")
    );
    // Verify pinned_at is still present after rename
    assert!(
        renamed_conv.metadata.get("pinned_at").is_some(),
        "pinned_at should still be present after rename"
    );
    println!("✅ Conversation renamed successfully, pinned_at preserved");

    // Step 3: Unpin the conversation
    let unpin_response = server
        .delete(format!("/v1/conversations/{}/pin", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(unpin_response.status_code(), 200);
    let unpinned_conv = unpin_response.json::<api::models::ConversationObject>();

    // Verify pinned_at is removed after unpinning (this was the bug)
    assert!(
        unpinned_conv.metadata.get("pinned_at").is_none(),
        "pinned_at should be removed from metadata after unpinning, even after rename"
    );
    // Verify title is still preserved
    assert_eq!(
        unpinned_conv.metadata.get("title").and_then(|v| v.as_str()),
        Some("Renamed Conversation")
    );
    println!("✅ Conversation unpinned successfully, pinned_at removed from metadata");
}

#[tokio::test]
async fn test_clone_conversation() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation with metadata
    let create_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": {
                "title": "Original Conversation",
                "custom_field": "custom_value"
            }
        }))
        .await;
    assert_eq!(create_response.status_code(), 201);
    let original_conv = create_response.json::<api::models::ConversationObject>();
    println!("Created original conversation: {}", original_conv.id);

    // Test: Clone the conversation
    let clone_response = server
        .post(format!("/v1/conversations/{}/clone", original_conv.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(clone_response.status_code(), 201);
    let cloned_conv = clone_response.json::<api::models::ConversationObject>();

    // Verify cloned conversation has different ID
    assert_ne!(cloned_conv.id, original_conv.id);
    println!("✅ Cloned conversation has new ID: {}", cloned_conv.id);

    // Verify cloned conversation has " (Copy)" appended to title
    let cloned_title = cloned_conv.metadata.get("title").and_then(|v| v.as_str());
    assert_eq!(cloned_title, Some("Original Conversation (Copy)"));
    println!("✅ Cloned conversation title has ' (Copy)' appended");

    // Verify other metadata is preserved
    assert_eq!(
        cloned_conv
            .metadata
            .get("custom_field")
            .and_then(|v| v.as_str()),
        Some("custom_value")
    );
    println!("✅ Other metadata preserved in clone");

    // Test: Clone without title in metadata
    let create_no_title_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": {
                "custom_field": "value"
            }
        }))
        .await;
    assert_eq!(create_no_title_response.status_code(), 201);
    let no_title_conv = create_no_title_response.json::<api::models::ConversationObject>();

    let clone_no_title_response = server
        .post(format!("/v1/conversations/{}/clone", no_title_conv.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(clone_no_title_response.status_code(), 201);
    let cloned_no_title = clone_no_title_response.json::<api::models::ConversationObject>();

    // Verify metadata is still copied even without title
    assert_eq!(
        cloned_no_title
            .metadata
            .get("custom_field")
            .and_then(|v| v.as_str()),
        Some("value")
    );
    println!("✅ Clone works correctly without title in metadata");
}

#[tokio::test]
async fn test_clone_conversation_with_responses_and_items() {
    let (server, _guard) = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation with metadata
    let conv_create_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": {
                "title": "Original Conversation with Messages",
                "description": "Test deep clone"
            }
        }))
        .await;
    assert_eq!(conv_create_response.status_code(), 201);
    let original_conv = conv_create_response.json::<api::models::ConversationObject>();
    println!("Created original conversation: {}", original_conv.id);

    // Add multiple responses to the conversation
    let models = list_models(&server, api_key.clone()).await;

    let response1 = create_response(
        &server,
        original_conv.id.clone(),
        models.data[0].id.clone(),
        "First message in conversation".to_string(),
        50,
        api_key.clone(),
    )
    .await;
    println!("Created response 1: {}", response1.id);

    let response2 = create_response(
        &server,
        original_conv.id.clone(),
        models.data[0].id.clone(),
        "Second message in conversation".to_string(),
        50,
        api_key.clone(),
    )
    .await;
    println!("Created response 2: {}", response2.id);

    // Get original conversation items count
    let original_items =
        list_conversation_items(&server, original_conv.id.clone(), api_key.clone()).await;
    let original_item_count = original_items.data.len();
    println!("Original conversation has {original_item_count} items");
    assert!(
        original_item_count >= 4,
        "Should have at least 4 items (2 user messages + 2 assistant responses)"
    );

    // Clone the conversation (deep clone)
    let clone_response = server
        .post(format!("/v1/conversations/{}/clone", original_conv.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(clone_response.status_code(), 201);
    let cloned_conv = clone_response.json::<api::models::ConversationObject>();
    println!("Cloned conversation: {}", cloned_conv.id);

    // Verify cloned conversation has different ID
    assert_ne!(cloned_conv.id, original_conv.id);

    // Verify title has " (Copy)" appended
    assert_eq!(
        cloned_conv.metadata.get("title").and_then(|v| v.as_str()),
        Some("Original Conversation with Messages (Copy)")
    );

    // Get cloned conversation items
    let cloned_items =
        list_conversation_items(&server, cloned_conv.id.clone(), api_key.clone()).await;
    let cloned_item_count = cloned_items.data.len();
    println!("Cloned conversation has {cloned_item_count} items");

    // Verify the clone has the same number of items as the original
    assert_eq!(
        cloned_item_count, original_item_count,
        "Cloned conversation should have the same number of items as original"
    );

    // Verify the content of items is the same (but with different IDs)
    for (orig_item, cloned_item) in original_items.data.iter().zip(cloned_items.data.iter()) {
        // IDs should be different
        assert_ne!(
            orig_item.id(),
            cloned_item.id(),
            "Item IDs should be different"
        );

        // Content should be the same
        if let (
            api::models::ConversationItem::Message {
                content: orig_content,
                role: orig_role,
                ..
            },
            api::models::ConversationItem::Message {
                content: cloned_content,
                role: cloned_role,
                ..
            },
        ) = (orig_item, cloned_item)
        {
            assert_eq!(orig_role, cloned_role, "Roles should match");
            assert_eq!(
                orig_content.len(),
                cloned_content.len(),
                "Content parts count should match"
            );

            // Compare text content
            for (orig_part, cloned_part) in orig_content.iter().zip(cloned_content.iter()) {
                match (orig_part, cloned_part) {
                    (
                        api::models::ConversationContentPart::InputText { text: orig_text },
                        api::models::ConversationContentPart::InputText { text: cloned_text },
                    ) => {
                        assert_eq!(orig_text, cloned_text, "Text content should match");
                    }
                    (
                        api::models::ConversationContentPart::OutputText {
                            text: orig_text, ..
                        },
                        api::models::ConversationContentPart::OutputText {
                            text: cloned_text, ..
                        },
                    ) => {
                        assert_eq!(orig_text, cloned_text, "Output text should match");
                    }
                    _ => {} // Other content types
                }
            }
        }
        // Other item types (tool calls, etc.)
    }

    println!("✅ Deep clone successfully copied all responses and items");
    println!("✅ Cloned items have different IDs but same content");

    // Verify that modifying the clone doesn't affect the original
    let update_clone = server
        .post(format!("/v1/conversations/{}", cloned_conv.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": {
                "title": "Modified Clone",
                "description": "Changed description"
            }
        }))
        .await;
    assert_eq!(update_clone.status_code(), 200);

    // Get original conversation to verify it wasn't changed
    let get_original = server
        .get(format!("/v1/conversations/{}", original_conv.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(get_original.status_code(), 200);
    let unchanged_original = get_original.json::<api::models::ConversationObject>();

    assert_eq!(
        unchanged_original
            .metadata
            .get("title")
            .and_then(|v| v.as_str()),
        Some("Original Conversation with Messages"),
        "Original conversation title should be unchanged"
    );

    println!("✅ Clone is independent - modifying clone doesn't affect original");
}

#[tokio::test]
async fn test_clone_pinned_and_archived_conversation() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let create_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": {
                "title": "Test Conversation",
                "custom_field": "custom_value"
            }
        }))
        .await;
    assert_eq!(create_response.status_code(), 201);
    let conversation = create_response.json::<api::models::ConversationObject>();
    println!("Created conversation: {}", conversation.id);

    // Pin the conversation
    let pin_response = server
        .post(format!("/v1/conversations/{}/pin", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(pin_response.status_code(), 200);
    println!("✅ Pinned conversation");

    // Archive the conversation
    let archive_response = server
        .post(format!("/v1/conversations/{}/archive", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(archive_response.status_code(), 200);
    let pinned_archived_conv = archive_response.json::<api::models::ConversationObject>();

    // Verify both pinned_at and archived_at are present
    assert!(
        pinned_archived_conv.metadata.get("pinned_at").is_some(),
        "Original should have pinned_at"
    );
    assert!(
        pinned_archived_conv.metadata.get("archived_at").is_some(),
        "Original should have archived_at"
    );
    println!("✅ Conversation is both pinned and archived");

    // Clone the pinned and archived conversation
    let clone_response = server
        .post(format!("/v1/conversations/{}/clone", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(clone_response.status_code(), 201);
    let cloned_conv = clone_response.json::<api::models::ConversationObject>();

    // Verify cloned conversation has different ID
    assert_ne!(cloned_conv.id, conversation.id);
    println!("✅ Cloned conversation has new ID: {}", cloned_conv.id);

    // Verify cloned conversation does NOT inherit pinned_at or archived_at
    assert!(
        cloned_conv.metadata.get("pinned_at").is_none(),
        "Clone should NOT inherit pinned_at from original"
    );
    assert!(
        cloned_conv.metadata.get("archived_at").is_none(),
        "Clone should NOT inherit archived_at from original"
    );

    // Verify other metadata is preserved
    assert_eq!(
        cloned_conv.metadata.get("title").and_then(|v| v.as_str()),
        Some("Test Conversation (Copy)"),
        "Clone should have title with (Copy) appended"
    );
    assert_eq!(
        cloned_conv
            .metadata
            .get("custom_field")
            .and_then(|v| v.as_str()),
        Some("custom_value"),
        "Clone should preserve other metadata fields"
    );

    println!("✅ Clone does not inherit pinned_at or archived_at");
    println!("✅ Clone starts fresh without pin/archive state");
}

#[tokio::test]
async fn test_delete_conversation() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;
    println!("Created conversation: {}", conversation.id);

    // Verify conversation exists
    let get_response = server
        .get(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(get_response.status_code(), 200);

    // Test: Delete the conversation
    let delete_response = server
        .delete(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(delete_response.status_code(), 200);
    let delete_result = delete_response.json::<api::models::ConversationDeleteResult>();
    assert_eq!(delete_result.id, conversation.id);
    assert_eq!(delete_result.object, "conversation.deleted");
    assert!(delete_result.deleted);
    println!("✅ Conversation deleted successfully");

    // Test: Getting deleted conversation should return 404
    let get_deleted_response = server
        .get(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(get_deleted_response.status_code(), 404);
    println!("✅ Deleted conversation returns 404");

    // Test: Deleting non-existent conversation should return 404
    let fake_id = "conv_00000000-0000-0000-0000-000000000000";
    let delete_fake_response = server
        .delete(format!("/v1/conversations/{fake_id}").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(delete_fake_response.status_code(), 404);
    println!("✅ Deleting non-existent conversation returns 404");
}

#[tokio::test]
async fn test_pin_nonexistent_conversation() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let fake_id = "conv_00000000-0000-0000-0000-000000000000";

    // Test: Pinning non-existent conversation should return 404
    let pin_response = server
        .post(format!("/v1/conversations/{fake_id}/pin").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(pin_response.status_code(), 404);
    println!("✅ Pinning non-existent conversation returns 404");
}

#[tokio::test]
async fn test_archive_nonexistent_conversation() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let fake_id = "conv_00000000-0000-0000-0000-000000000000";

    // Test: Archiving non-existent conversation should return 404
    let archive_response = server
        .post(format!("/v1/conversations/{fake_id}/archive").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(archive_response.status_code(), 404);
    println!("✅ Archiving non-existent conversation returns 404");
}

#[tokio::test]
async fn test_clone_nonexistent_conversation() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let fake_id = "conv_00000000-0000-0000-0000-000000000000";

    // Test: Cloning non-existent conversation should return 404
    let clone_response = server
        .post(format!("/v1/conversations/{fake_id}/clone").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(clone_response.status_code(), 404);
    println!("✅ Cloning non-existent conversation returns 404");
}

#[tokio::test]
async fn test_pin_and_archive_together() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;
    println!("Created conversation: {}", conversation.id);

    // Pin the conversation first
    let pin_now = chrono::Utc::now().timestamp();
    let pin_response = server
        .post(format!("/v1/conversations/{}/pin", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(pin_response.status_code(), 200);
    let pinned_conv = pin_response.json::<api::models::ConversationObject>();

    // Verify pinned_at is present
    assert!(pinned_conv.metadata.get("pinned_at").is_some());
    assert!(pinned_conv.metadata.get("archived_at").is_none());
    println!("✅ Conversation pinned");

    // Archive the pinned conversation
    let archive_now = chrono::Utc::now().timestamp();
    let archive_response = server
        .post(format!("/v1/conversations/{}/archive", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(archive_response.status_code(), 200);
    let archived_pinned_conv = archive_response.json::<api::models::ConversationObject>();

    // Verify both pinned_at and archived_at are present
    let pinned_at = archived_pinned_conv
        .metadata
        .get("pinned_at")
        .expect("pinned_at should still be present after archiving")
        .as_i64()
        .expect("pinned_at should be a number");
    let archived_at = archived_pinned_conv
        .metadata
        .get("archived_at")
        .expect("archived_at should be present")
        .as_i64()
        .expect("archived_at should be a number");

    assert!(
        pinned_at >= pin_now,
        "pinned_at should be from when conversation was pinned"
    );
    assert!(
        archived_at >= archive_now,
        "archived_at should be from when conversation was archived"
    );
    println!("✅ Conversation can be both pinned and archived");

    // Unpin the archived conversation
    let unpin_response = server
        .delete(format!("/v1/conversations/{}/pin", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(unpin_response.status_code(), 200);
    let unpinned_archived_conv = unpin_response.json::<api::models::ConversationObject>();

    // Verify pinned_at is removed but archived_at remains
    assert!(
        unpinned_archived_conv.metadata.get("pinned_at").is_none(),
        "pinned_at should be removed after unpinning"
    );
    assert!(
        unpinned_archived_conv.metadata.get("archived_at").is_some(),
        "archived_at should still be present"
    );
    println!("✅ Unpinning removes pinned_at but keeps archived_at");

    // Unarchive to clean up
    let unarchive_response = server
        .delete(format!("/v1/conversations/{}/archive", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(unarchive_response.status_code(), 200);
    let unarchived_conv = unarchive_response.json::<api::models::ConversationObject>();

    // Verify both are removed
    assert!(
        unarchived_conv.metadata.get("pinned_at").is_none(),
        "pinned_at should not be present"
    );
    assert!(
        unarchived_conv.metadata.get("archived_at").is_none(),
        "archived_at should be removed after unarchiving"
    );
    println!("✅ Unarchiving removes archived_at");
}

#[tokio::test]
async fn test_combined_conversation_operations() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation with metadata
    let create_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": {
                "title": "Test Conversation",
                "tags": ["important", "work"]
            }
        }))
        .await;
    assert_eq!(create_response.status_code(), 201);
    let conversation = create_response.json::<api::models::ConversationObject>();
    println!("Created conversation: {}", conversation.id);

    // Pin the conversation
    let pin_response = server
        .post(format!("/v1/conversations/{}/pin", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(pin_response.status_code(), 200);
    let pinned_conv = pin_response.json::<api::models::ConversationObject>();

    // Verify pinned_at is present
    assert!(
        pinned_conv.metadata.get("pinned_at").is_some(),
        "pinned_at should be present after pinning"
    );
    println!("✅ Pinned conversation with timestamp");

    // Update metadata (rename)
    let update_response = server
        .post(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": {
                "title": "Renamed Conversation",
                "tags": ["important", "work", "updated"]
            }
        }))
        .await;
    assert_eq!(update_response.status_code(), 200);
    println!("✅ Updated metadata");

    // Clone the conversation
    let clone_response = server
        .post(format!("/v1/conversations/{}/clone", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(clone_response.status_code(), 201);
    let cloned_conv = clone_response.json::<api::models::ConversationObject>();

    // Verify clone has updated metadata with " (Copy)" appended
    assert_eq!(
        cloned_conv.metadata.get("title").and_then(|v| v.as_str()),
        Some("Renamed Conversation (Copy)")
    );
    println!("✅ Cloned conversation has correct title");

    // Archive the original conversation
    let archive_response = server
        .post(format!("/v1/conversations/{}/archive", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(archive_response.status_code(), 200);
    let archived_conv = archive_response.json::<api::models::ConversationObject>();

    // Verify archived_at is present
    assert!(
        archived_conv.metadata.get("archived_at").is_some(),
        "archived_at should be present after archiving"
    );
    // pinned_at should also still be present from earlier pin operation
    assert!(
        archived_conv.metadata.get("pinned_at").is_some(),
        "pinned_at should still be present after archiving"
    );
    println!("✅ Archived original conversation with timestamp");

    // Delete the cloned conversation
    let delete_response = server
        .delete(format!("/v1/conversations/{}", cloned_conv.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(delete_response.status_code(), 200);
    println!("✅ Deleted cloned conversation");

    // Original conversation should still exist (just archived and pinned)
    let get_response = server
        .get(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(get_response.status_code(), 200);
    println!("✅ Original conversation still exists after operations");

    println!("✅ All combined operations completed successfully");
}

#[tokio::test]
async fn test_conversation_metadata_limits() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Test: Create conversation with metadata containing multiple key-value pairs
    let mut metadata = serde_json::Map::new();
    for i in 0..16 {
        metadata.insert(
            format!("key{i}"),
            serde_json::Value::String(format!("value{i}")),
        );
    }

    let create_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": metadata
        }))
        .await;
    assert_eq!(create_response.status_code(), 201);
    let conversation = create_response.json::<api::models::ConversationObject>();

    // Verify all metadata keys are present
    assert_eq!(conversation.metadata.as_object().unwrap().len(), 16);
    println!("✅ Conversation created with 16 metadata keys (OpenAI limit)");

    // Note: OpenAI spec allows max 16 key-value pairs
    // We're not enforcing this limit at the database level, but documenting it
}

#[tokio::test]
async fn test_conversation_operations_with_invalid_id_format() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let invalid_id = "not-a-valid-uuid";

    // Test: All operations with invalid ID format should return 400
    let pin_response = server
        .post(format!("/v1/conversations/{invalid_id}/pin").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(pin_response.status_code(), 400);

    let archive_response = server
        .post(format!("/v1/conversations/{invalid_id}/archive").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(archive_response.status_code(), 400);

    let clone_response = server
        .post(format!("/v1/conversations/{invalid_id}/clone").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(clone_response.status_code(), 400);

    let delete_response = server
        .delete(format!("/v1/conversations/{invalid_id}").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(delete_response.status_code(), 400);

    println!("✅ All operations with invalid ID format return 400");
}

#[tokio::test]
async fn test_conversation_unauthorized_access() {
    let (server, _guard) = setup_test_server().await;
    let (api_key1, _) = create_org_and_api_key(&server).await;
    let (api_key2, _) = create_org_and_api_key(&server).await;

    // Create conversation with first API key
    let conversation = create_conversation(&server, api_key1.clone()).await;
    println!("Created conversation with API key 1: {}", conversation.id);

    // Test: Try to pin conversation with different API key (different workspace)
    let pin_response = server
        .post(format!("/v1/conversations/{}/pin", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key2}"))
        .await;
    assert_eq!(pin_response.status_code(), 404); // Should not find conversation in different workspace
    println!("✅ Cross-workspace pin attempt returns 404");

    // Test: Try to clone conversation with different API key
    let clone_response = server
        .post(format!("/v1/conversations/{}/clone", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key2}"))
        .await;
    assert_eq!(clone_response.status_code(), 404);
    println!("✅ Cross-workspace clone attempt returns 404");

    // Test: Try to delete conversation with different API key
    let delete_response = server
        .delete(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key2}"))
        .await;
    assert_eq!(delete_response.status_code(), 404);
    println!("✅ Cross-workspace delete attempt returns 404");

    // Verify conversation still exists with original API key
    let get_response = server
        .get(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key1}"))
        .await;
    assert_eq!(get_response.status_code(), 200);
    println!("✅ Original conversation still accessible with correct API key");
}

#[tokio::test]
async fn test_conversation_items_include_model() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Use a specific model
    let model_name = "Qwen/Qwen3-30B-A3B-Instruct-2507";

    // Create a response with the specific model
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "What is Rust?",
            "temperature": 0.7,
            "max_output_tokens": 100,
            "stream": false,
            "model": model_name,
            "conversation": conversation.id
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let response_obj = response.json::<api::models::ResponseObject>();
    println!("Created response: {}", response_obj.id);

    // List conversation items
    let items_response = server
        .get(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(items_response.status_code(), 200);
    let items_list = items_response.json::<api::models::ConversationItemList>();

    println!("Retrieved {} conversation items", items_list.data.len());

    // Verify each item has the model field populated
    for item in &items_list.data {
        match item {
            api::models::ConversationItem::Message {
                id, model, role, ..
            } => {
                println!("  Message item {id}: role={role}, model={model}");

                // Verify model field is populated and matches the model used for the response
                assert!(!model.is_empty(), "Model field should not be empty");
                assert_eq!(
                    model, model_name,
                    "Model field should match the model used for the response"
                );
            }
            api::models::ConversationItem::ToolCall { id, model, .. } => {
                println!("  ToolCall item {id}: model={model}");
                assert!(!model.is_empty(), "Model field should not be empty");
                assert_eq!(
                    model, model_name,
                    "Model field should match the model used for the response"
                );
            }
            api::models::ConversationItem::WebSearchCall { id, model, .. } => {
                println!("  WebSearchCall item {id}: model={model}");
                assert!(!model.is_empty(), "Model field should not be empty");
                assert_eq!(
                    model, model_name,
                    "Model field should match the model used for the response"
                );
            }
            api::models::ConversationItem::Reasoning { id, model, .. } => {
                println!("  Reasoning item {id}: model={model}");
                assert!(!model.is_empty(), "Model field should not be empty");
                assert_eq!(
                    model, model_name,
                    "Model field should match the model used for the response"
                );
            }
            // MCP items don't have model field in the API model
            api::models::ConversationItem::McpListTools { id, .. } => {
                println!("  McpListTools item {id}");
                assert!(!id.is_empty(), "McpListTools id should not be empty");
            }
            api::models::ConversationItem::McpCall { id, .. } => {
                println!("  McpCall item {id}");
                assert!(!id.is_empty(), "McpCall id should not be empty");
            }
            api::models::ConversationItem::McpApprovalRequest { id, .. } => {
                println!("  McpApprovalRequest item {id}");
                assert!(!id.is_empty(), "McpApprovalRequest id should not be empty");
            }
        }
    }

    println!("✅ All conversation items include the model field");
    println!("✅ Model field matches the model used for the response: {model_name}");
}

#[tokio::test]
async fn test_conversation_items_model_with_streaming() {
    let (server, _guard) = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Use a specific model
    let model_name = "Qwen/Qwen3-30B-A3B-Instruct-2507";

    // Create a streaming response with the specific model
    let (_, response_obj) = create_response_stream(
        &server,
        conversation.id.clone(),
        model_name.to_string(),
        "What is Rust?".to_string(),
        100,
        api_key.clone(),
    )
    .await;

    println!("Created streaming response: {}", response_obj.id);

    // List conversation items
    let items_response = server
        .get(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(items_response.status_code(), 200);
    let items_list = items_response.json::<api::models::ConversationItemList>();

    println!(
        "Retrieved {} conversation items from streaming response",
        items_list.data.len()
    );

    // Verify each item has the model field populated
    let mut found_user_message = false;
    let mut found_assistant_message = false;

    for item in &items_list.data {
        if let api::models::ConversationItem::Message {
            id, model, role, ..
        } = item
        {
            println!("  Message item {id}: role={role}, model={model}");

            // Verify model field is populated and matches the model used for the response
            assert!(!model.is_empty(), "Model field should not be empty");
            assert_eq!(
                model, model_name,
                "Model field should match the model used for the response"
            );

            if role == "user" {
                found_user_message = true;
            } else if role == "assistant" {
                found_assistant_message = true;
            }
        }
    }

    // Verify we found both user and assistant messages
    assert!(
        found_user_message,
        "Should have found a user message with model field"
    );
    assert!(
        found_assistant_message,
        "Should have found an assistant message with model field"
    );

    println!("✅ All conversation items from streaming response include the model field");
    println!("✅ Model field matches the model used for the response: {model_name}");
}

#[tokio::test]
async fn test_backfilled_items_include_model() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Use a specific model for backfilling
    let model_name = "Qwen/Qwen3-30B-A3B-Instruct-2507";

    // Backfill some conversation items (this creates items directly without a response)
    let create_items_response = server
        .post(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "items": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Hello!"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "input_text", "text": "Hi there!"}]
                }
            ]
        }))
        .await;

    assert_eq!(create_items_response.status_code(), 200);
    let items_response = create_items_response.json::<api::models::ConversationItemList>();
    assert_eq!(items_response.data.len(), 2);

    // Now create a response in the same conversation with a specific model
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "What is Rust?",
            "temperature": 0.7,
            "max_output_tokens": 100,
            "stream": false,
            "model": model_name,
            "conversation": conversation.id
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // List all conversation items
    let items_list_response = server
        .get(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(items_list_response.status_code(), 200);
    let items_list = items_list_response.json::<api::models::ConversationItemList>();

    println!(
        "Retrieved {} total conversation items (backfilled + response)",
        items_list.data.len()
    );

    // Verify all items have the model field populated
    for item in &items_list.data {
        if let api::models::ConversationItem::Message {
            id, model, role, ..
        } = item
        {
            println!("  Message item {id}: role={role}, model={model}");

            // Verify model field is populated
            // Note: Backfilled items get their model from the response they're associated with
            // All items in this test should have the same model since they're all in the same response chain
            assert!(
                !model.is_empty(),
                "Model field should not be empty for item {id}"
            );
        }
    }

    println!("✅ All conversation items (including backfilled) include the model field");
}

#[tokio::test]
async fn test_batch_get_conversations() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create 3 conversations
    let conv1 = create_conversation(&server, api_key.clone()).await;
    let conv2 = create_conversation(&server, api_key.clone()).await;
    let conv3 = create_conversation(&server, api_key.clone()).await;

    println!(
        "✅ Created 3 conversations: {}, {}, {}",
        conv1.id, conv2.id, conv3.id
    );

    // Create 2 fake conversation IDs that don't exist (using hyphenated UUID format for consistency)
    let fake_conv1_id = "conv_00000000-0000-0000-0000-000000000000";
    let fake_conv2_id = "conv_11111111-1111-1111-1111-111111111111";

    println!("📝 Using 2 missing conversation IDs: {fake_conv1_id}, {fake_conv2_id}");

    // Batch get 5 conversations (3 real, 2 missing)
    let batch_request = serde_json::json!({
        "ids": [
            conv1.id.clone(),
            conv2.id.clone(),
            conv3.id.clone(),
            fake_conv1_id,
            fake_conv2_id,
        ]
    });

    let response = server
        .post("/v1/conversations/batch")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&batch_request)
        .await;

    println!("📡 Batch request status: {}", response.status_code());
    assert_eq!(
        response.status_code(),
        200,
        "Expected 200 OK, got: {}",
        response.status_code()
    );

    let batch_response = response.json::<api::models::ConversationBatchResponse>();

    println!("✅ Response parsed successfully");

    // Verify response structure
    assert_eq!(batch_response.object, "list", "object should be 'list'");
    println!("✅ Response object type: {}", batch_response.object);

    // Verify we got 3 conversations in data
    assert_eq!(
        batch_response.data.len(),
        3,
        "Expected 3 conversations in data, got {}",
        batch_response.data.len()
    );
    println!(
        "✅ Found {} conversations in data (expected 3)",
        batch_response.data.len()
    );

    // Verify we got 2 missing IDs
    assert_eq!(
        batch_response.missing_ids.len(),
        2,
        "Expected 2 missing IDs, got {}",
        batch_response.missing_ids.len()
    );
    println!(
        "✅ Found {} missing IDs (expected 2)",
        batch_response.missing_ids.len()
    );

    // Verify the found conversations match what we created
    let found_ids: std::collections::HashSet<String> =
        batch_response.data.iter().map(|c| c.id.clone()).collect();

    assert!(
        found_ids.contains(&conv1.id),
        "conv1 ({}) should be in results",
        conv1.id
    );
    assert!(
        found_ids.contains(&conv2.id),
        "conv2 ({}) should be in results",
        conv2.id
    );
    assert!(
        found_ids.contains(&conv3.id),
        "conv3 ({}) should be in results",
        conv3.id
    );
    println!(
        "✅ All 3 created conversations are in the results: {}, {}, {}",
        conv1.id, conv2.id, conv3.id
    );

    // Verify ordering: returned conversations should match the order of requested IDs
    // Expected order in request: conv1, conv2, conv3 (then 2 missing)
    // So returned conversations should be in that same order
    assert_eq!(
        batch_response.data[0].id, conv1.id,
        "First returned conversation should be conv1 (requested first)"
    );
    assert_eq!(
        batch_response.data[1].id, conv2.id,
        "Second returned conversation should be conv2 (requested second)"
    );
    assert_eq!(
        batch_response.data[2].id, conv3.id,
        "Third returned conversation should be conv3 (requested third)"
    );
    println!("✅ Returned conversations are in the same order as requested");

    // Verify the missing IDs are correct and returned in original format
    let missing_ids_set: std::collections::HashSet<String> =
        batch_response.missing_ids.iter().cloned().collect();

    assert!(
        missing_ids_set.contains(fake_conv1_id),
        "fake_conv1 ({fake_conv1_id}) should be in missing_ids, got: {:?}",
        batch_response.missing_ids
    );
    assert!(
        missing_ids_set.contains(fake_conv2_id),
        "fake_conv2 ({fake_conv2_id}) should be in missing_ids, got: {:?}",
        batch_response.missing_ids
    );
    println!("✅ Both missing IDs are correctly listed in original format: {fake_conv1_id}, {fake_conv2_id}");

    // Verify missing_ids ordering is preserved from request
    // Expected order in request: fake_conv1_id, fake_conv2_id (after the 3 real ones)
    assert_eq!(
        batch_response.missing_ids[0], fake_conv1_id,
        "First missing ID should be fake_conv1 (requested 4th)"
    );
    assert_eq!(
        batch_response.missing_ids[1], fake_conv2_id,
        "Second missing ID should be fake_conv2 (requested 5th)"
    );
    println!("✅ Missing IDs are in the same order as requested and in original format");

    // Verify each conversation object has required fields
    for (idx, conv) in batch_response.data.iter().enumerate() {
        assert!(
            !conv.id.is_empty(),
            "Conversation {idx} should have non-empty id"
        );
        assert!(
            !conv.object.is_empty(),
            "Conversation {idx} should have non-empty object"
        );
        assert!(
            conv.created_at != 0,
            "Conversation {idx} should have non-zero created_at"
        );
        println!(
            "✅ Conversation {idx}: id={}, object={}, created_at={}",
            conv.id, conv.object, conv.created_at
        );
    }

    println!("✅ Batch conversation retrieval test passed!");
}

#[tokio::test]
async fn test_conversation_title_strips_thinking_tags() {
    use inference_providers::mock::ResponseTemplate;

    let (server, _pool, mock_provider, _db, _guard) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_default_response(ResponseTemplate::new(
            "<think>Let me analyze this message to generate a title...</think>Test Conversation Title",
        ))
        .await;

    let response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "name": "test-conv-title-strip",
            "description": "Testing title generation with thinking model"
        }))
        .await;
    assert_eq!(response.status_code(), 201);
    let conversation = response.json::<api::models::ConversationObject>();

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation.id,
            },
            "input": "Hello, this is a test message for title generation",
            "temperature": 0.7,
            "max_output_tokens": 50,
            "stream": true,
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507"
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    let response_text = response.text();
    let mut title_from_event: Option<String> = None;

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

        if event_type == "conversation.title.updated" && !event_data.is_empty() {
            if let Ok(event_json) = serde_json::from_str::<serde_json::Value>(event_data) {
                if let Some(title) = event_json
                    .get("conversation_title")
                    .and_then(|v| v.as_str())
                {
                    title_from_event = Some(title.to_string());
                }
            }
        }
    }

    let title = match title_from_event {
        Some(t) => t,
        None => {
            let conv_response = server
                .get(format!("/v1/conversations/{}", conversation.id).as_str())
                .add_header("Authorization", format!("Bearer {api_key}"))
                .await;
            assert_eq!(conv_response.status_code(), 200);
            let updated_conv = conv_response.json::<api::models::ConversationObject>();
            updated_conv
                .metadata
                .get("title")
                .and_then(|v| v.as_str())
                .expect("Expected title in event or metadata")
                .to_string()
        }
    };

    assert!(
        !title.contains("<think>"),
        "Title should not contain <think> tags, got: {title}"
    );
    assert!(
        !title.contains("</think>"),
        "Title should not contain </think> tags, got: {title}"
    );
    println!("✅ Title stripped thinking tags: {title}");
}

#[tokio::test]
async fn test_chat_completions_with_json_schema() {
    use common::mock_prompts;
    use inference_providers::mock::{RequestMatcher, ResponseTemplate};

    let (server, _pool, mock, _db, _guard) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Configure mock to match exact prompt and return structured JSON
    // Chat completions API sends messages directly without language instruction
    let user_message = "Generate a user profile";
    let expected_prompt = mock_prompts::build_simple_prompt(user_message);
    let expected_json = r#"{"name": "Alice Johnson", "age": 28, "email": "alice@example.com"}"#;

    mock.when(RequestMatcher::ExactPrompt(expected_prompt))
        .respond_with(ResponseTemplate::new(expected_json))
        .await;

    // Make a chat completion request with response_format
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [
                {
                    "role": "user",
                    "content": user_message
                }
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "user_profile",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "age": {"type": "integer"},
                            "email": {"type": "string"}
                        },
                        "required": ["name", "age", "email"]
                    },
                    "strict": true
                }
            }
        }))
        .await;

    if response.status_code() != 200 {
        let error = response.text();
        println!("Error response: {}", error);
    }
    assert_eq!(response.status_code(), 200);
    let completion = response.json::<serde_json::Value>();

    // Verify the response contains the expected JSON
    let content = completion["choices"][0]["message"]["content"]
        .as_str()
        .expect("Expected content in response");

    assert_eq!(content, expected_json);

    // Verify it's valid JSON matching the schema
    let json_obj: serde_json::Value =
        serde_json::from_str(content).expect("Content should be valid JSON");
    assert_eq!(json_obj["name"], "Alice Johnson");
    assert_eq!(json_obj["age"], 28);
    assert_eq!(json_obj["email"], "alice@example.com");

    println!("✅ Chat completions with JSON schema returned structured output");
}

#[tokio::test]
async fn test_responses_api_with_json_schema() {
    use common::mock_prompts;
    use inference_providers::mock::{RequestMatcher, ResponseTemplate};

    let (server, _pool, mock, _db, _guard) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Configure mock to match exact prompt and return structured JSON
    let user_message = "Generate a book description";
    let expected_prompt = mock_prompts::build_prompt(user_message);
    let expected_json = r#"{"title": "The Great Adventure", "author": "John Smith", "year": 2024, "genre": "Fiction"}"#;

    mock.when(RequestMatcher::ExactPrompt(expected_prompt))
        .respond_with(ResponseTemplate::new(expected_json))
        .await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Make a response request with text.format.json_schema
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation.id,
            },
            "input": user_message,
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "max_output_tokens": 100,
            "stream": false,
            "text": {
                "format": {
                    "type": "json_schema",
                    "json_schema": {
                        "name": "book",
                        "schema": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"},
                                "author": {"type": "string"},
                                "year": {"type": "integer"},
                                "genre": {"type": "string"}
                            },
                            "required": ["title", "author", "year", "genre"]
                        },
                        "strict": true
                    }
                }
            }
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let response_obj = response.json::<api::models::ResponseObject>();

    // Verify the response contains structured output
    assert!(!response_obj.output.is_empty());

    if let ResponseOutputItem::Message { content, .. } = &response_obj.output[0] {
        assert!(!content.is_empty());

        if let ResponseOutputContent::OutputText { text, .. } = &content[0] {
            // Verify it's valid JSON matching the schema
            let json_obj: serde_json::Value =
                serde_json::from_str(text).expect("Content should be valid JSON");
            assert_eq!(json_obj["title"], "The Great Adventure");
            assert_eq!(json_obj["author"], "John Smith");
            assert_eq!(json_obj["year"], 2024);
            assert_eq!(json_obj["genre"], "Fiction");

            println!("✅ Responses API with JSON schema returned structured output");
        } else {
            panic!("Expected OutputText content");
        }
    } else {
        panic!("Expected Message output item");
    }
}
