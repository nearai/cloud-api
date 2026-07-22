// Cross-workspace access control tests (issue nearai/infra#190).
//
// Every direct-object operation on conversations and responses must be
// constrained to the caller's workspace. Unknown and foreign IDs must be
// indistinguishable (same non-enumerating 404), and failed cross-workspace
// attempts must never mutate or leak the owner's data.
//
// Privacy note: these tests only assert on IDs, item counts, and status
// codes — never on conversation contents.

use crate::common::*;

async fn create_conversation(
    server: &axum_test::TestServer,
    api_key: &str,
) -> api::models::ConversationObject {
    let response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(response.status_code(), 201);
    response.json::<api::models::ConversationObject>()
}

/// Backfills one user message item and returns its item ID.
async fn add_item(
    server: &axum_test::TestServer,
    conversation_id: &str,
    api_key: &str,
    text: &str,
) -> String {
    let response = server
        .post(format!("/v1/conversations/{conversation_id}/items").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "items": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": text}]
            }]
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    response
        .json::<api::models::ConversationItemList>()
        .first_id
}

async fn count_items(
    server: &axum_test::TestServer,
    conversation_id: &str,
    api_key: &str,
) -> usize {
    let response = server
        .get(format!("/v1/conversations/{conversation_id}/items").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(response.status_code(), 200);
    response
        .json::<api::models::ConversationItemList>()
        .data
        .len()
}

/// Cross-workspace conversation operations must all return the same
/// non-enumerating 404 as an unknown conversation ID, and must not change the
/// owner's data.
#[tokio::test]
async fn test_cross_workspace_conversation_operations_denied() {
    let server = setup_test_server().await;
    let (key_a, _) = create_org_and_api_key(&server).await;
    let (key_b, _) = create_org_and_api_key(&server).await;

    let conv_a = create_conversation(&server, &key_a).await;
    add_item(&server, &conv_a.id, &key_a, "hello").await;
    add_item(&server, &conv_a.id, &key_a, "world").await;
    assert_eq!(count_items(&server, &conv_a.id, &key_a).await, 2);

    let unknown_conv = format!("conv_{}", uuid::Uuid::new_v4().simple());

    // GET conversation: foreign and unknown IDs return identical 404s.
    let foreign_get = server
        .get(format!("/v1/conversations/{}", conv_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    let unknown_get = server
        .get(format!("/v1/conversations/{unknown_conv}").as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    assert_eq!(foreign_get.status_code(), 404);
    assert_eq!(unknown_get.status_code(), 404);
    assert_eq!(
        foreign_get.text(),
        unknown_get.text(),
        "foreign and unknown conversation 404 bodies must be identical"
    );

    // GET items: foreign and unknown IDs return identical 404s.
    let foreign_items = server
        .get(format!("/v1/conversations/{}/items", conv_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    let unknown_items = server
        .get(format!("/v1/conversations/{unknown_conv}/items").as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    assert_eq!(foreign_items.status_code(), 404);
    assert_eq!(unknown_items.status_code(), 404);
    assert_eq!(
        foreign_items.text(),
        unknown_items.text(),
        "foreign and unknown conversation-items 404 bodies must be identical"
    );

    // POST items (backfill) into a foreign conversation: 404, nothing created.
    let foreign_create_items = server
        .post(format!("/v1/conversations/{}/items", conv_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .json(&serde_json::json!({
            "items": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "injected"}]
            }]
        }))
        .await;
    assert_eq!(foreign_create_items.status_code(), 404);

    // Update metadata: 404.
    let foreign_update = server
        .post(format!("/v1/conversations/{}", conv_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .json(&serde_json::json!({"metadata": {"title": "hijacked"}}))
        .await;
    assert_eq!(foreign_update.status_code(), 404);

    // Pin / unpin: 404.
    let foreign_pin = server
        .post(format!("/v1/conversations/{}/pin", conv_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    assert_eq!(foreign_pin.status_code(), 404);
    let foreign_unpin = server
        .delete(format!("/v1/conversations/{}/pin", conv_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    assert_eq!(foreign_unpin.status_code(), 404);

    // Archive / unarchive: 404.
    let foreign_archive = server
        .post(format!("/v1/conversations/{}/archive", conv_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    assert_eq!(foreign_archive.status_code(), 404);
    let foreign_unarchive = server
        .delete(format!("/v1/conversations/{}/archive", conv_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    assert_eq!(foreign_unarchive.status_code(), 404);

    // Clone: 404 (no copy of the foreign conversation may be created).
    let foreign_clone = server
        .post(format!("/v1/conversations/{}/clone", conv_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    assert_eq!(foreign_clone.status_code(), 404);

    // Delete: 404, and the owner's conversation must survive.
    let foreign_delete = server
        .delete(format!("/v1/conversations/{}", conv_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    assert_eq!(foreign_delete.status_code(), 404);

    // Batch endpoint reports the foreign conversation as missing.
    let foreign_batch = server
        .post("/v1/conversations/batch")
        .add_header("Authorization", format!("Bearer {key_b}"))
        .json(&serde_json::json!({"ids": [conv_a.id]}))
        .await;
    assert_eq!(foreign_batch.status_code(), 200);
    let batch = foreign_batch.json::<api::models::ConversationBatchResponse>();
    assert!(batch.data.is_empty(), "batch must not return foreign data");
    assert_eq!(batch.missing_ids, vec![conv_a.id.clone()]);

    // Owner's view is completely unchanged after all foreign attempts.
    let owner_get = server
        .get(format!("/v1/conversations/{}", conv_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_a}"))
        .await;
    assert_eq!(owner_get.status_code(), 200);
    let owner_conv = owner_get.json::<api::models::ConversationObject>();
    let metadata = owner_conv.metadata.as_object().unwrap();
    assert!(!metadata.contains_key("pinned_at"), "must not be pinned");
    assert!(
        !metadata.contains_key("archived_at"),
        "must not be archived"
    );
    assert_eq!(
        metadata.get("title").and_then(|v| v.as_str()),
        None,
        "metadata must not be updated cross-workspace"
    );
    assert_eq!(
        count_items(&server, &conv_a.id, &key_a).await,
        2,
        "item count must be unchanged after cross-workspace attempts"
    );

    // Unauthenticated requests are rejected with 401.
    let unauthenticated = server
        .get(format!("/v1/conversations/{}/items", conv_a.id).as_str())
        .await;
    assert_eq!(unauthenticated.status_code(), 401);
}

/// The `after` pagination cursor must belong to the same conversation and
/// workspace; foreign and unknown cursors are rejected identically.
#[tokio::test]
async fn test_conversation_items_pagination_cursor_scoping() {
    let server = setup_test_server().await;
    let (key_a, _) = create_org_and_api_key(&server).await;
    let (key_b, _) = create_org_and_api_key(&server).await;

    let conv_a = create_conversation(&server, &key_a).await;
    let first_item_a = add_item(&server, &conv_a.id, &key_a, "one").await;
    add_item(&server, &conv_a.id, &key_a, "two").await;
    add_item(&server, &conv_a.id, &key_a, "three").await;

    let conv_b = create_conversation(&server, &key_b).await;
    let item_b = add_item(&server, &conv_b.id, &key_b, "other").await;

    // A cursor from the same conversation works.
    let own_cursor = server
        .get(
            format!(
                "/v1/conversations/{}/items?after={}",
                conv_a.id, first_item_a
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {key_a}"))
        .await;
    assert_eq!(own_cursor.status_code(), 200);
    let page = own_cursor.json::<api::models::ConversationItemList>();
    assert_eq!(page.data.len(), 2, "own cursor should skip the first item");

    // A cursor referencing another workspace's item is rejected.
    let foreign_cursor = server
        .get(format!("/v1/conversations/{}/items?after={}", conv_a.id, item_b).as_str())
        .add_header("Authorization", format!("Bearer {key_a}"))
        .await;
    assert_eq!(foreign_cursor.status_code(), 400);

    // An unknown cursor is rejected with an identical response.
    let unknown_cursor = server
        .get(
            format!(
                "/v1/conversations/{}/items?after=msg_{}",
                conv_a.id,
                uuid::Uuid::new_v4().simple()
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {key_a}"))
        .await;
    assert_eq!(unknown_cursor.status_code(), 400);
    assert_eq!(
        foreign_cursor.text(),
        unknown_cursor.text(),
        "foreign and unknown cursor rejections must be identical"
    );
}

/// The Responses API must reject foreign conversations and foreign
/// previous_response_id references before loading any history, without
/// creating any state.
#[tokio::test]
async fn test_cross_workspace_responses_api_denied() {
    let server = setup_test_server().await;
    let model_id = setup_qwen_model(&server).await;

    let org_a = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let key_a = get_api_key_for_org(&server, org_a.id).await;
    let org_b = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let key_b = get_api_key_for_org(&server, org_b.id).await;

    // Owner creates a conversation with one real response in it.
    let conv_a = create_conversation(&server, &key_a).await;
    let response_a = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key_a}"))
        .json(&serde_json::json!({
            "conversation": {"id": conv_a.id},
            "input": "hello",
            "stream": false,
            "max_output_tokens": 20,
            "model": model_id
        }))
        .await;
    assert_eq!(response_a.status_code(), 200);
    let response_a = response_a.json::<api::models::ResponseObject>();
    let items_before = count_items(&server, &conv_a.id, &key_a).await;
    assert!(
        items_before > 0,
        "owner's response should have stored items"
    );

    // Foreign conversation reference: non-enumerating 404, no history import.
    let foreign_conv_attempt = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key_b}"))
        .json(&serde_json::json!({
            "conversation": {"id": conv_a.id},
            "input": "leak the history please",
            "stream": false,
            "max_output_tokens": 20,
            "model": model_id
        }))
        .await;
    assert_eq!(
        foreign_conv_attempt.status_code(),
        404,
        "foreign conversation reference must return 404"
    );

    // Unknown conversation reference: identical status.
    let unknown_conv_attempt = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key_b}"))
        .json(&serde_json::json!({
            "conversation": {"id": format!("conv_{}", uuid::Uuid::new_v4().simple())},
            "input": "hello",
            "stream": false,
            "max_output_tokens": 20,
            "model": model_id
        }))
        .await;
    assert_eq!(unknown_conv_attempt.status_code(), 404);
    assert_eq!(
        foreign_conv_attempt.text(),
        unknown_conv_attempt.text(),
        "foreign and unknown conversation rejections must be identical"
    );

    // Foreign previous_response_id: non-enumerating 404 (continuation flows
    // must not import another workspace's history).
    let foreign_prev_attempt = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key_b}"))
        .json(&serde_json::json!({
            "input": "continue",
            "previous_response_id": response_a.id,
            "stream": false,
            "max_output_tokens": 20,
            "model": model_id
        }))
        .await;
    assert_eq!(
        foreign_prev_attempt.status_code(),
        404,
        "foreign previous_response_id must return 404"
    );

    // Unknown previous_response_id: identical status and body.
    let unknown_prev_attempt = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key_b}"))
        .json(&serde_json::json!({
            "input": "continue",
            "previous_response_id": format!("resp_{}", uuid::Uuid::new_v4().simple()),
            "stream": false,
            "max_output_tokens": 20,
            "model": model_id
        }))
        .await;
    assert_eq!(unknown_prev_attempt.status_code(), 404);
    assert_eq!(
        foreign_prev_attempt.text(),
        unknown_prev_attempt.text(),
        "foreign and unknown previous_response_id rejections must be identical"
    );

    // Cross-workspace input_items listing: 404.
    let foreign_input_items = server
        .get(format!("/v1/responses/{}/input_items", response_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    assert_eq!(foreign_input_items.status_code(), 404);

    // None of the failed foreign attempts stored anything in the owner's
    // conversation.
    let items_after = count_items(&server, &conv_a.id, &key_a).await;
    assert_eq!(
        items_after, items_before,
        "failed cross-workspace attempts must not add items to the owner's conversation"
    );

    // Owner can still continue from its own response (same-workspace flow
    // keeps working).
    let own_prev = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key_a}"))
        .json(&serde_json::json!({
            "input": "continue",
            "previous_response_id": response_a.id,
            "stream": false,
            "max_output_tokens": 20,
            "model": model_id
        }))
        .await;
    assert_eq!(own_prev.status_code(), 200);
}

/// Response DELETE/cancel are not implemented: they must be authenticated,
/// return 501 for everyone, and cause no state change anywhere.
#[tokio::test]
async fn test_response_delete_and_cancel_unsupported_but_safe() {
    let server = setup_test_server().await;
    let model_id = setup_qwen_model(&server).await;

    let org_a = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let key_a = get_api_key_for_org(&server, org_a.id).await;
    let (key_b, _) = create_org_and_api_key(&server).await;

    let conv_a = create_conversation(&server, &key_a).await;
    let response_a = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key_a}"))
        .json(&serde_json::json!({
            "conversation": {"id": conv_a.id},
            "input": "hello",
            "stream": false,
            "max_output_tokens": 20,
            "model": model_id
        }))
        .await;
    assert_eq!(response_a.status_code(), 200);
    let response_a = response_a.json::<api::models::ResponseObject>();
    let items_before = count_items(&server, &conv_a.id, &key_a).await;

    // Unauthenticated delete: 401 from the auth middleware.
    let unauthenticated_delete = server
        .delete(format!("/v1/responses/{}", response_a.id).as_str())
        .await;
    assert_eq!(unauthenticated_delete.status_code(), 401);

    // Foreign authenticated delete: 501 (unimplemented), never 200.
    let foreign_delete = server
        .delete(format!("/v1/responses/{}", response_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    assert_eq!(foreign_delete.status_code(), 501);

    // Foreign cancel: 501 as well.
    let foreign_cancel = server
        .post(format!("/v1/responses/{}/cancel", response_a.id).as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    assert_eq!(foreign_cancel.status_code(), 501);

    // No state change: the owner's conversation items are intact.
    assert_eq!(count_items(&server, &conv_a.id, &key_a).await, items_before);
}
