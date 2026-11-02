use super::*;

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

    // Unknown conversations return the same non-enumerating 404 as
    // conversations owned by another workspace (see issue nearai/infra#190).
    assert_eq!(create_items_response.status_code(), 404);
    let error_response = create_items_response.json::<api::models::ErrorResponse>();
    assert_eq!(error_response.error.message, "Conversation not found");

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
async fn test_create_conversation_items_with_file_content() {
    let server = setup_test_server().await;
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
