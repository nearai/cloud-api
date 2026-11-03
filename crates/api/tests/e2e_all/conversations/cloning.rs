use super::*;

#[tokio::test]
async fn test_clone_conversation() {
    let server = setup_test_server().await;
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
    let (server, database) = setup_test_server_with_database().await;
    let model_id = setup_qwen_model(&server).await;
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

    seed_response(
        &database,
        &original_conv.id,
        &model_id,
        "First message in conversation",
        None,
        false,
    )
    .await;
    seed_response(
        &database,
        &original_conv.id,
        &model_id,
        "Second message in conversation",
        None,
        false,
    )
    .await;
    seed_response(
        &database,
        &original_conv.id,
        &model_id,
        "Response with confidential fields",
        None,
        true,
    )
    .await;

    // Get original conversation items count
    let original_items =
        list_conversation_items(&server, original_conv.id.clone(), api_key.clone()).await;
    let original_item_count = original_items.data.len();
    println!("Original conversation has {original_item_count} items");
    assert!(
        original_item_count == 6,
        "All three retained turns must be available to clone"
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

    let original_uuid = uuid::Uuid::parse_str(
        original_conv
            .id
            .strip_prefix("conv_")
            .unwrap_or(&original_conv.id),
    )
    .expect("original conversation UUID");
    let cloned_uuid = uuid::Uuid::parse_str(
        cloned_conv
            .id
            .strip_prefix("conv_")
            .unwrap_or(&cloned_conv.id),
    )
    .expect("cloned conversation UUID");
    let client = database.pool().get().await.expect("database connection");
    for conversation_id in [original_uuid, cloned_uuid] {
        let metadata: serde_json::Value = client
            .query_one(
                "SELECT metadata FROM conversations WHERE id=$1",
                &[&conversation_id],
            )
            .await
            .expect("stored conversation metadata")
            .get(0);
        assert_eq!(metadata[database::field_encryption::MARKER], true);

        let response_row = client
            .query_one(
                "SELECT instructions,metadata FROM responses WHERE conversation_id=$1 AND instructions IS NOT NULL LIMIT 1",
                &[&conversation_id],
            )
            .await
            .expect("stored confidential response fields");
        assert!(response_row
            .get::<_, String>("instructions")
            .contains(database::field_encryption::MARKER));
        assert_eq!(
            response_row.get::<_, serde_json::Value>("metadata")
                [database::field_encryption::MARKER],
            true
        );
    }
    drop(client);

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
    let server = setup_test_server().await;
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
async fn test_create_and_clone_conversation_return_root_response_id() {
    let (server, database) = setup_test_server_with_database().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create conversation: response metadata must include root_response_id for first-turn parallel responses
    let create_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({ "metadata": {} }))
        .await;
    assert_eq!(create_response.status_code(), 201);
    let created = create_response.json::<api::models::ConversationObject>();
    let meta = created.metadata.as_object().unwrap();
    let root_id = meta
        .get("root_response_id")
        .and_then(|v| v.as_str())
        .expect("create conversation response must include metadata.root_response_id");
    assert!(
        root_id.starts_with("resp_"),
        "root_response_id must be a response ID (resp_*), got {root_id}"
    );
    println!(
        "✅ Create conversation returns root_response_id: {}",
        root_id
    );

    // Simulate a root written by an old replica after V0076 has run: the legacy
    // metadata marker exists, but the new structural flag retains its default.
    let root_uuid =
        Uuid::parse_str(root_id.trim_start_matches("resp_")).expect("root response UUID");
    let client = database.pool().get().await.expect("database connection");
    client
        .execute(
            "UPDATE responses SET metadata='{\"root_response\":true}'::jsonb, is_root_response=FALSE WHERE id=$1",
            &[&root_uuid],
        )
        .await
        .expect("simulate root created by an old replica");
    drop(client);

    // Clone conversation: response metadata must include root_response_id (cloned conversation has its own root)
    let clone_response = server
        .post(format!("/v1/conversations/{}/clone", created.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(clone_response.status_code(), 201);
    let cloned = clone_response.json::<api::models::ConversationObject>();
    let cloned_meta = cloned.metadata.as_object().unwrap();
    let cloned_root_id = cloned_meta
        .get("root_response_id")
        .and_then(|v| v.as_str())
        .expect("clone conversation response must include metadata.root_response_id");
    assert!(
        cloned_root_id.starts_with("resp_"),
        "cloned root_response_id must be resp_*, got {cloned_root_id}"
    );
    assert_ne!(
        cloned_root_id, root_id,
        "cloned conversation must have a different root_response_id than the original"
    );

    let cloned_conversation_uuid =
        Uuid::parse_str(cloned.id.trim_start_matches("conv_")).expect("cloned conversation UUID");
    let client = database.pool().get().await.expect("database connection");
    let structural_roots: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM responses WHERE conversation_id=$1 AND is_root_response",
            &[&cloned_conversation_uuid],
        )
        .await
        .expect("count cloned structural roots")
        .get(0);
    assert_eq!(
        structural_roots, 1,
        "the cloned legacy root must be repaired instead of duplicated"
    );
    println!(
        "✅ Clone conversation returns root_response_id: {}",
        cloned_root_id
    );
}
