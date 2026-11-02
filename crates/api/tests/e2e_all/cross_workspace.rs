//! Workspace isolation for retained conversation reads, deletion, and pagination.
//! Direct repository fixtures keep this coverage independent of public writes.

use crate::common::*;

async fn seed_workspace_history(
    server: &axum_test::TestServer,
    database: &std::sync::Arc<database::Database>,
    item_count: usize,
) -> (String, String, Vec<String>) {
    let org = create_org(server).await;
    let key = get_api_key_for_org(server, org.id.clone()).await;
    let (conversation, item_ids) =
        seed_conversation_history(database, org.id.parse().unwrap(), item_count).await;
    (key, conversation.id.to_string(), item_ids)
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

/// Retained conversation reads and deletion must return the same
/// non-enumerating 404 as an unknown conversation ID, and must not change the
/// owner's data.
#[tokio::test]
async fn retained_conversation_reads_and_delete_are_workspace_scoped() {
    let (server, database) = setup_test_server_with_database().await;
    let (key_a, conv_a, _) = seed_workspace_history(&server, &database, 2).await;
    let (key_b, _) = create_org_and_api_key(&server).await;
    assert_eq!(count_items(&server, &conv_a, &key_a).await, 2);

    let unknown_conv = format!("conv_{}", uuid::Uuid::new_v4().simple());

    // GET conversation: foreign and unknown IDs return identical 404s.
    let foreign_get = server
        .get(format!("/v1/conversations/{conv_a}").as_str())
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
        .get(format!("/v1/conversations/{conv_a}/items").as_str())
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

    // Delete: 404, and the owner's conversation must survive.
    let foreign_delete = server
        .delete(format!("/v1/conversations/{conv_a}").as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    assert_eq!(foreign_delete.status_code(), 404);
    let unknown_delete = server
        .delete(format!("/v1/conversations/{unknown_conv}").as_str())
        .add_header("Authorization", format!("Bearer {key_b}"))
        .await;
    assert_eq!(unknown_delete.status_code(), 404);
    assert_eq!(foreign_delete.text(), unknown_delete.text());

    // Batch endpoint reports the foreign conversation as missing.
    let foreign_batch = server
        .post("/v1/conversations/batch")
        .add_header("Authorization", format!("Bearer {key_b}"))
        .json(&serde_json::json!({"ids": [conv_a]}))
        .await;
    assert_eq!(foreign_batch.status_code(), 200);
    let batch = foreign_batch.json::<api::models::ConversationBatchResponse>();
    assert!(batch.data.is_empty(), "batch must not return foreign data");
    assert_eq!(batch.missing_ids, vec![conv_a.clone()]);

    // Owner's view is completely unchanged after all foreign attempts.
    let owner_get = server
        .get(format!("/v1/conversations/{conv_a}").as_str())
        .add_header("Authorization", format!("Bearer {key_a}"))
        .await;
    assert_eq!(owner_get.status_code(), 200);
    assert_eq!(
        count_items(&server, &conv_a, &key_a).await,
        2,
        "item count must be unchanged after cross-workspace attempts"
    );

    // Unauthenticated requests are rejected with 401.
    let unauthenticated = server
        .get(format!("/v1/conversations/{conv_a}/items").as_str())
        .await;
    assert_eq!(unauthenticated.status_code(), 401);
}

/// The `after` pagination cursor must belong to the same conversation and
/// workspace; foreign and unknown cursors are rejected identically.
#[tokio::test]
async fn test_conversation_items_pagination_cursor_scoping() {
    let (server, database) = setup_test_server_with_database().await;
    let (key_a, conv_a, items_a) = seed_workspace_history(&server, &database, 3).await;
    let (_key_b, _conv_b, items_b) = seed_workspace_history(&server, &database, 1).await;
    let first_item_a = &items_a[0];
    let item_b = &items_b[0];

    // A cursor from the same conversation works.
    let own_cursor = server
        .get(format!("/v1/conversations/{conv_a}/items?after={first_item_a}").as_str())
        .add_header("Authorization", format!("Bearer {key_a}"))
        .await;
    assert_eq!(own_cursor.status_code(), 200);
    let page = own_cursor.json::<api::models::ConversationItemList>();
    assert_eq!(page.data.len(), 2, "own cursor should skip the first item");

    // A cursor referencing another workspace's item is rejected.
    let foreign_cursor = server
        .get(format!("/v1/conversations/{conv_a}/items?after={item_b}").as_str())
        .add_header("Authorization", format!("Bearer {key_a}"))
        .await;
    assert_eq!(foreign_cursor.status_code(), 400);

    // An unknown cursor is rejected with an identical response.
    let unknown_cursor = server
        .get(
            format!(
                "/v1/conversations/{}/items?after=msg_{}",
                conv_a,
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
