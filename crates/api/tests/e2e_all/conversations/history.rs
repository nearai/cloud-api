use super::*;

#[tokio::test]
async fn test_conversation_items_pagination() {
    let (server, database) = setup_test_server_with_database().await;
    let model_id = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    // Retained responses are fixtures: new Responses requests no longer persist history.
    for i in 0..5 {
        seed_response(
            &database,
            &conversation.id,
            &model_id,
            &format!("Test message {}", i + 1),
            None,
            false,
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
            ConversationItem::FunctionCall { id, .. } => id.clone(),
            ConversationItem::FunctionCallOutput { id, .. } => id.clone(),
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
async fn test_conversation_items_include_response_metadata() {
    let (server, database) = setup_test_server_with_database().await;
    let model_name = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    let parent_response = seed_response(
        &database,
        &conversation.id,
        &model_name,
        "What is Rust?",
        None,
        false,
    )
    .await;
    let follow_up_response = seed_response(
        &database,
        &conversation.id,
        &model_name,
        "Tell me more about that.",
        Some(&parent_response.id),
        false,
    )
    .await;

    // List conversation items
    let items_response = server
        .get(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(items_response.status_code(), 200);
    let items_list = items_response.json::<api::models::ConversationItemList>();

    println!("Retrieved {} conversation items", items_list.data.len());

    assert_eq!(
        items_list.data.len(),
        4,
        "both retained turns must be visible"
    );

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
            api::models::ConversationItem::FunctionCall { created_at, .. } => Some(*created_at),
            api::models::ConversationItem::FunctionCallOutput { created_at, .. } => {
                Some(*created_at)
            }
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

#[tokio::test]
async fn test_conversation_items_include_model() {
    let (server, database) = setup_test_server_with_database().await;
    let model_name = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

    seed_response(
        &database,
        &conversation.id,
        &model_name,
        "What is Rust?",
        None,
        false,
    )
    .await;

    // List conversation items
    let items_response = server
        .get(format!("/v1/conversations/{}/items", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(items_response.status_code(), 200);
    let items_list = items_response.json::<api::models::ConversationItemList>();

    println!("Retrieved {} conversation items", items_list.data.len());

    assert_eq!(
        items_list.data.len(),
        2,
        "both retained messages must be visible"
    );
    assert!(items_list
        .data
        .iter()
        .any(|item| matches!(item, ConversationItem::Message { role, .. } if role == "user")));
    assert!(items_list
        .data
        .iter()
        .any(|item| matches!(item, ConversationItem::Message { role, .. } if role == "assistant")));

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
                    model, &model_name,
                    "Model field should match the model used for the response"
                );
            }
            api::models::ConversationItem::ToolCall { id, model, .. } => {
                println!("  ToolCall item {id}: model={model}");
                assert!(!model.is_empty(), "Model field should not be empty");
                assert_eq!(
                    model, &model_name,
                    "Model field should match the model used for the response"
                );
            }
            api::models::ConversationItem::WebSearchCall { id, model, .. } => {
                println!("  WebSearchCall item {id}: model={model}");
                assert!(!model.is_empty(), "Model field should not be empty");
                assert_eq!(
                    model, &model_name,
                    "Model field should match the model used for the response"
                );
            }
            api::models::ConversationItem::Reasoning { id, model, .. } => {
                println!("  Reasoning item {id}: model={model}");
                assert!(!model.is_empty(), "Model field should not be empty");
                assert_eq!(
                    model, &model_name,
                    "Model field should match the model used for the response"
                );
            }
            api::models::ConversationItem::FunctionCall { id, model, .. } => {
                println!("  FunctionCall item {id}: model={model}");
                assert!(!model.is_empty(), "Model field should not be empty");
                assert_eq!(
                    model, &model_name,
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
            api::models::ConversationItem::FunctionCallOutput { id, .. } => {
                println!("  FunctionCallOutput item {id}");
                assert!(!id.is_empty(), "FunctionCallOutput id should not be empty");
            }
        }
    }

    println!("✅ All conversation items include the model field");
    println!("✅ Model field matches the model used for the response: {model_name}");
}

#[tokio::test]
async fn test_backfilled_items_include_model() {
    let (server, database) = setup_test_server_with_database().await;
    let model_name = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;

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

    // Existing response history and freshly backfilled items remain readable together.
    seed_response(
        &database,
        &conversation.id,
        &model_name,
        "What is Rust?",
        None,
        false,
    )
    .await;

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

    assert_eq!(
        items_list.data.len(),
        4,
        "backfilled and retained messages must be visible"
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
