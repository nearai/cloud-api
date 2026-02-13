// E2E tests for message-level metadata in Response API
mod common;

use common::*;
use serde_json::json;

/// Test that input message metadata is preserved through create response and retrieved via list_input_items
#[tokio::test]
async fn test_input_message_metadata_preserved() {
    let (server, _, _mock, _db, _guard) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Setup a model for testing
    let model = setup_qwen_model(&server).await;

    // Create a conversation first
    let conversation = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "name": "Test Conversation"
        }))
        .await;
    assert_eq!(conversation.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation.json();

    // Create response with input message that has metadata
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": model,
            "conversation": {
                "id": conversation.id,
            },
            "input": [{
                "role": "user",
                "content": "Hello, world!",
                "metadata": {
                    "source": "test",
                    "custom_key": "custom_value",
                    "nested": {"foo": "bar"}
                }
            }],
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let response_obj: api::models::ResponseObject = response.json();

    // List input items and verify metadata is preserved
    let input_items_response = server
        .get(format!("/v1/responses/{}/input_items", response_obj.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(input_items_response.status_code(), 200);
    let input_items: api::models::ResponseInputItemList = input_items_response.json();

    assert_eq!(input_items.data.len(), 1);
    let input_item = &input_items.data[0];
    assert_eq!(input_item.role, "user");

    // Verify metadata was preserved
    let metadata = input_item
        .metadata
        .as_ref()
        .expect("metadata should be present");
    assert_eq!(metadata["source"], "test");
    assert_eq!(metadata["custom_key"], "custom_value");
    assert_eq!(metadata["nested"]["foo"], "bar");
}

/// Test that input message without metadata works (backward compatibility)
#[tokio::test]
async fn test_input_message_without_metadata() {
    let (server, _, _mock, _db, _guard) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = setup_qwen_model(&server).await;

    // Create a conversation
    let conversation = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "name": "Test Conversation"
        }))
        .await;
    assert_eq!(conversation.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation.json();

    // Create response with input message without metadata (old format)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": model,
            "conversation": {
                "id": conversation.id,
            },
            "input": [{
                "role": "user",
                "content": "Hello without metadata"
            }],
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let response_obj: api::models::ResponseObject = response.json();

    // List input items and verify it works without metadata
    let input_items_response = server
        .get(format!("/v1/responses/{}/input_items", response_obj.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(input_items_response.status_code(), 200);
    let input_items: api::models::ResponseInputItemList = input_items_response.json();

    assert_eq!(input_items.data.len(), 1);
    let input_item = &input_items.data[0];
    assert_eq!(input_item.role, "user");

    // Metadata should be None when not provided
    assert!(
        input_item.metadata.is_none(),
        "metadata should be None when not provided"
    );
}

/// Test that oversized input message metadata is rejected
#[tokio::test]
async fn test_input_message_metadata_size_limit() {
    let (server, _, _mock, _db, _guard) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = setup_qwen_model(&server).await;

    // Create a conversation
    let conversation = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "name": "Test Conversation"
        }))
        .await;
    assert_eq!(conversation.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation.json();

    // Create large metadata that exceeds 16KB limit
    let large_string = "x".repeat(17 * 1024); // 17KB

    // Create response with oversized metadata
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": model,
            "conversation": {
                "id": conversation.id,
            },
            "input": [{
                "role": "user",
                "content": "Hello",
                "metadata": {
                    "large_field": large_string
                }
            }],
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    // Should be rejected with 400 Bad Request
    assert_eq!(response.status_code(), 400);
    let error: api::models::ErrorResponse = response.json();
    assert!(
        error.error.message.contains("metadata is too large"),
        "Error message should mention metadata size, got: {}",
        error.error.message
    );
}

/// Test that simple text input still works (without metadata support)
#[tokio::test]
async fn test_simple_text_input_no_metadata() {
    let (server, _, _mock, _db, _guard) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = setup_qwen_model(&server).await;

    // Create a conversation
    let conversation = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "name": "Test Conversation"
        }))
        .await;
    assert_eq!(conversation.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation.json();

    // Create response with simple text input (not array format)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": model,
            "conversation": {
                "id": conversation.id,
            },
            "input": "Simple text input",
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let response_obj: api::models::ResponseObject = response.json();

    // List input items
    let input_items_response = server
        .get(format!("/v1/responses/{}/input_items", response_obj.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(input_items_response.status_code(), 200);
    let input_items: api::models::ResponseInputItemList = input_items_response.json();

    assert_eq!(input_items.data.len(), 1);
    let input_item = &input_items.data[0];
    assert_eq!(input_item.role, "user");

    // Simple text input cannot carry metadata
    assert!(
        input_item.metadata.is_none(),
        "Simple text input should not have metadata"
    );
}

/// Test that conversation items include metadata from user messages
#[tokio::test]
async fn test_conversation_items_include_user_message_metadata() {
    let (server, _, _mock, _db, _guard) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = setup_qwen_model(&server).await;

    // Create a conversation
    let conversation = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "name": "Test Conversation"
        }))
        .await;
    assert_eq!(conversation.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation.json();

    // Create response with input message that has metadata
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": model,
            "conversation": {
                "id": conversation.id,
            },
            "input": [{
                "role": "user",
                "content": "Hello, world!",
                "metadata": {
                    "author_id": "user123",
                    "author_name": "Test User",
                    "source": "test",
                    "custom_key": "custom_value"
                }
            }],
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let _response_obj: api::models::ResponseObject = response.json();

    // List conversation items and verify metadata is present
    let items_response = server
        .get(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(items_response.status_code(), 200);
    let items: api::models::ConversationItemList = items_response.json();

    // Find the user message item
    let user_message = items
        .data
        .iter()
        .find(|item| matches!(item, api::models::ConversationItem::Message { role, .. } if role == "user"))
        .expect("Should find user message in conversation items");

    if let api::models::ConversationItem::Message { metadata, .. } = user_message {
        let metadata = metadata
            .as_ref()
            .expect("User message should have metadata");

        // Verify author metadata is present
        assert_eq!(metadata["author_id"], "user123");
        assert_eq!(metadata["author_name"], "Test User");
        assert_eq!(metadata["source"], "test");
        assert_eq!(metadata["custom_key"], "custom_value");
    } else {
        panic!("Expected Message item");
    }
}

/// Test that conversation items include metadata from multiple messages
#[tokio::test]
async fn test_conversation_items_include_multiple_message_metadata() {
    let (server, _, _mock, _db, _guard) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = setup_qwen_model(&server).await;

    // Create a conversation
    let conversation = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "name": "Test Conversation"
        }))
        .await;
    assert_eq!(conversation.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation.json();

    // Create first response with metadata
    let response1 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": model,
            "conversation": {
                "id": conversation.id,
            },
            "input": [{
                "role": "user",
                "content": "First message",
                "metadata": {
                    "author_id": "user1",
                    "author_name": "User One",
                    "message_id": "msg1"
                }
            }],
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    assert_eq!(response1.status_code(), 200);
    let response1_obj: api::models::ResponseObject = response1.json();

    // Create second response with different metadata
    let response2 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": model,
            "conversation": {
                "id": conversation.id,
            },
            "previous_response_id": response1_obj.id,
            "input": [{
                "role": "user",
                "content": "Second message",
                "metadata": {
                    "author_id": "user2",
                    "author_name": "User Two",
                    "message_id": "msg2"
                }
            }],
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    assert_eq!(response2.status_code(), 200);

    // List conversation items and verify both messages have their metadata
    let items_response = server
        .get(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(items_response.status_code(), 200);
    let items: api::models::ConversationItemList = items_response.json();

    // Find all user messages
    let user_messages: Vec<_> = items
        .data
        .iter()
        .filter_map(|item| match item {
            api::models::ConversationItem::Message {
                role,
                metadata,
                content,
                ..
            } if role == "user" => Some((content, metadata)),
            _ => None,
        })
        .collect();

    assert_eq!(user_messages.len(), 2, "Should have 2 user messages");

    // Verify first message metadata
    let first_msg = &user_messages[0];
    let first_metadata = first_msg
        .1
        .as_ref()
        .expect("First message should have metadata");
    assert_eq!(first_metadata["author_id"], "user1");
    assert_eq!(first_metadata["author_name"], "User One");
    assert_eq!(first_metadata["message_id"], "msg1");

    // Verify second message metadata
    let second_msg = &user_messages[1];
    let second_metadata = second_msg
        .1
        .as_ref()
        .expect("Second message should have metadata");
    assert_eq!(second_metadata["author_id"], "user2");
    assert_eq!(second_metadata["author_name"], "User Two");
    assert_eq!(second_metadata["message_id"], "msg2");
}

/// Test that conversation items preserve metadata from request-level metadata
#[tokio::test]
async fn test_conversation_items_include_request_metadata() {
    let (server, _, _mock, _db, _guard) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = setup_qwen_model(&server).await;

    // Create a conversation
    let conversation = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "name": "Test Conversation"
        }))
        .await;
    assert_eq!(conversation.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation.json();

    // Create response with request-level metadata (for simple text input)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": model,
            "conversation": {
                "id": conversation.id,
            },
            "input": "Simple text with request metadata",
            "metadata": {
                "author_id": "request_user",
                "author_name": "Request User"
            },
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let _response_obj: api::models::ResponseObject = response.json();

    // List conversation items and verify request metadata is included
    let items_response = server
        .get(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(items_response.status_code(), 200);
    let items: api::models::ConversationItemList = items_response.json();

    // Find the user message item
    let user_message = items
        .data
        .iter()
        .find(|item| matches!(item, api::models::ConversationItem::Message { role, .. } if role == "user"))
        .expect("Should find user message in conversation items");

    if let api::models::ConversationItem::Message { metadata, .. } = user_message {
        let metadata = metadata
            .as_ref()
            .expect("User message should have metadata from request");

        // Verify author metadata from request is present
        assert_eq!(metadata["author_id"], "request_user");
        assert_eq!(metadata["author_name"], "Request User");
    } else {
        panic!("Expected Message item");
    }
}

/// Test that conversation items without metadata work correctly
#[tokio::test]
async fn test_conversation_items_without_metadata() {
    let (server, _, _mock, _db, _guard) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = setup_qwen_model(&server).await;

    // Create a conversation
    let conversation = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "name": "Test Conversation"
        }))
        .await;
    assert_eq!(conversation.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation.json();

    // Create response without any metadata
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": model,
            "conversation": {
                "id": conversation.id,
            },
            "input": [{
                "role": "user",
                "content": "Message without metadata"
            }],
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let _response_obj: api::models::ResponseObject = response.json();

    // List conversation items
    let items_response = server
        .get(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(items_response.status_code(), 200);
    let items: api::models::ConversationItemList = items_response.json();

    // Find the user message item
    let user_message = items
        .data
        .iter()
        .find(|item| matches!(item, api::models::ConversationItem::Message { role, .. } if role == "user"))
        .expect("Should find user message in conversation items");

    if let api::models::ConversationItem::Message { metadata, .. } = user_message {
        // Metadata should be None when not provided
        assert!(
            metadata.is_none(),
            "Metadata should be None when not provided, got: {:?}",
            metadata
        );
    } else {
        panic!("Expected Message item");
    }
}

/// Test that item-level metadata takes precedence over request-level metadata
#[tokio::test]
async fn test_conversation_items_metadata_precedence() {
    let (server, _, _mock, _db, _guard) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = setup_qwen_model(&server).await;

    // Create a conversation
    let conversation = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "name": "Test Conversation"
        }))
        .await;
    assert_eq!(conversation.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation.json();

    // Create a response with both request-level and item-level metadata
    // Item-level metadata should take precedence
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": model,
            "conversation": {
                "id": conversation.id,
            },
            "metadata": {
                "author_id": "request-author-id",
                "author_name": "Request Author",
                "request_field": "request-value"
            },
            "input": [{
                "role": "user",
                "content": "Hello",
                "metadata": {
                    "author_id": "item-author-id",
                    "author_name": "Item Author",
                    "item_field": "item-value"
                }
            }],
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let _response_obj: api::models::ResponseObject = response.json();

    // List conversation items and verify metadata precedence
    let items_response = server
        .get(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(items_response.status_code(), 200);
    let items: api::models::ConversationItemList = items_response.json();

    // Find the user message
    let user_message = items
        .data
        .iter()
        .find(|item| {
            if let api::models::ConversationItem::Message { role, .. } = item {
                role == "user"
            } else {
                false
            }
        })
        .expect("Should find user message in conversation items");

    if let api::models::ConversationItem::Message { metadata, .. } = user_message {
        // Item-level metadata should take precedence
        let metadata = metadata.as_ref().expect("Metadata should be present");

        // Item-level fields should be present
        assert_eq!(
            metadata["author_id"], "item-author-id",
            "Item-level author_id should take precedence"
        );
        assert_eq!(
            metadata["author_name"], "Item Author",
            "Item-level author_name should take precedence"
        );
        assert_eq!(
            metadata["item_field"], "item-value",
            "Item-level field should be present"
        );

        // Request-level fields that don't conflict should NOT be present
        // (because item-level metadata replaces the entire metadata object)
        assert!(
            metadata.get("request_field").is_none(),
            "Request-level field should not be present when item-level metadata is provided"
        );
    } else {
        panic!("Expected Message item");
    }
}

/// Test that oversized metadata on create_conversation_items is rejected
#[tokio::test]
async fn test_create_conversation_items_metadata_size_limit() {
    let (server, _guard) = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a conversation
    let conversation = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "name": "Test Conversation"
        }))
        .await;
    assert_eq!(conversation.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation.json();

    // Build metadata that exceeds the 16KB limit
    let large_string = "x".repeat(17 * 1024);

    // POST /v1/conversations/{id}/items with oversized metadata
    let response = server
        .post(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "items": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Hello"}],
                    "metadata": { "big": large_string }
                }
            ]
        }))
        .await;

    assert_eq!(response.status_code(), 400);
    let error: api::models::ErrorResponse = response.json();
    assert!(
        error.error.message.contains("metadata is too large"),
        "Error should mention metadata size, got: {}",
        error.error.message
    );
}
