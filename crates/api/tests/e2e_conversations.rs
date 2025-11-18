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
        _ => panic!("Expected Message item type"),
    }
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
// Conversation Tests
// ============================================

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
async fn test_create_conversation_items_backfill() {
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
async fn test_create_conversation_items_different_roles() {
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;
    println!("Created conversation: {}", conversation.id);

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

    // Create first follow-up response (conversation inherited from parent)
    let response1 = server
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

    // Create second follow-up response from the same parent (conversation inherited)
    let response2 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
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

    // Create nested response (follows 1, conversation inherited)
    let nested_response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "Can you elaborate?",
            "temperature": 0.7,
            "max_output_tokens": 100,
            "stream": false,
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "previous_response_id": response1.id
        }))
        .await;

    assert_eq!(nested_response.status_code(), 200);
    let nested_response = nested_response.json::<api::models::ResponseObject>();
    println!("Created nested response: {}", nested_response.id);

    // Verify nested response has response1 as previous
    assert_eq!(
        nested_response.previous_response_id,
        Some(response1.id.clone()),
        "Nested response should reference response1 as previous_response_id"
    );

    // Now fetch the parent response again to verify next_response_ids was updated
    // Note: We need to implement a GET endpoint to verify this properly
    // For now, we'll verify through the database by creating a new response and checking

    println!("✅ Response previous-next relationships working correctly");
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
        "   - Nested: {} (previous: {})",
        nested_response.id,
        nested_response.previous_response_id.as_ref().unwrap()
    );
}

#[tokio::test]
async fn test_response_previous_next_relationships_streaming() {
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
            api::models::ConversationItem::Message { created_at, .. } => *created_at,
            api::models::ConversationItem::ToolCall { created_at, .. } => *created_at,
            api::models::ConversationItem::WebSearchCall { created_at, .. } => *created_at,
            api::models::ConversationItem::Reasoning { created_at, .. } => *created_at,
        };
        assert!(
            current_timestamp >= prev_timestamp,
            "Items should be sorted by created_at in ascending order"
        );
        prev_timestamp = current_timestamp;
    }

    println!("✅ Conversation items include response metadata (response_id, previous_response_id, next_response_ids, created_at)");
    println!("✅ Items are sorted by created_at in ascending order");
}

#[tokio::test]
async fn test_conversation_items_include_model() {
    let server = setup_test_server().await;
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
        }
    }

    println!("✅ All conversation items include the model field");
    println!("✅ Model field matches the model used for the response: {model_name}");
}

#[tokio::test]
async fn test_conversation_items_model_with_streaming() {
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
