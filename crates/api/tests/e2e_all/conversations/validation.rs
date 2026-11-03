use super::*;

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
async fn test_pin_nonexistent_conversation() {
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
async fn test_conversation_metadata_limits() {
    let server = setup_test_server().await;
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

    let meta = conversation.metadata.as_object().unwrap();
    // All 16 user-provided keys must be present (OpenAI limit)
    for i in 0..16 {
        let key = format!("key{i}");
        let expected = format!("value{i}");
        assert_eq!(
            meta.get(&key).and_then(|v| v.as_str()),
            Some(expected.as_str()),
            "key{i} missing or wrong value"
        );
    }
    // Response may include system keys (e.g. root_response_id for new conversations)
    assert!(
        meta.len() >= 16,
        "metadata must contain at least the 16 user keys, got {}",
        meta.len()
    );
    println!("✅ Conversation created with 16 metadata keys (OpenAI limit)");

    // Note: OpenAI spec allows max 16 key-value pairs
    // We're not enforcing this limit at the database level, but documenting it
}

#[tokio::test]
async fn test_conversation_operations_with_invalid_id_format() {
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
async fn test_batch_get_conversations() {
    let server = setup_test_server().await;
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
