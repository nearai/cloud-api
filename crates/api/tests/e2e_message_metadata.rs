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
