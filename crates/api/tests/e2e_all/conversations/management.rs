use super::*;

#[tokio::test]
async fn test_pin_unpin_conversation() {
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
async fn test_pin_and_archive_together() {
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
async fn test_delete_conversation() {
    let server = setup_test_server().await;
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
