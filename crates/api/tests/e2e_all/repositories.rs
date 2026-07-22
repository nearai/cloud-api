// E2E tests for Repository-level database operations
// These tests directly test repository behavior with the database

use chrono::{Duration, Utc};
use database::OAuthStateRepository;

async fn get_test_pool() -> database::pool::DbPool {
    let (_server, _inference_provider_pool, _mock_provider, database) =
        crate::common::setup_test_server_with_pool().await;
    database.pool().clone()
}

// ============================================
// OAuth State Repository Tests
// ============================================

#[tokio::test]
async fn test_create_and_get_oauth_state() {
    let pool = get_test_pool().await;
    let repo = OAuthStateRepository::new(pool.clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let provider = "github".to_string();

    // Create state
    let created = repo
        .create(state.clone(), provider.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(created.state, state);
    assert_eq!(created.provider, provider);
    assert_eq!(created.pkce_verifier, None);
    assert_eq!(created.frontend_callback, None);

    // Get and delete state
    let retrieved = repo.get_and_delete(&state).await.unwrap();
    assert!(retrieved.is_some());
    let retrieved = retrieved.unwrap();
    assert_eq!(retrieved.state, state);
    assert_eq!(retrieved.provider, provider);

    // Second get should return None (state was deleted)
    let second_get = repo.get_and_delete(&state).await.unwrap();
    assert!(second_get.is_none());
}

#[tokio::test]
async fn test_expired_state_not_returned() {
    let pool = get_test_pool().await;
    let repo = OAuthStateRepository::new(pool.clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());

    // Create state with past expiration
    let client = pool.get().await.unwrap();
    let past_time = Utc::now() - Duration::minutes(1);
    client
        .execute(
            r#"
            INSERT INTO oauth_states (state, provider, pkce_verifier, created_at, expires_at)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            &[&state, &"github", &None::<String>, &past_time, &past_time],
        )
        .await
        .unwrap();

    // Try to get expired state
    let result = repo.get_and_delete(&state).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn test_google_with_pkce_verifier() {
    let pool = get_test_pool().await;
    let repo = OAuthStateRepository::new(pool.clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let provider = "google".to_string();
    let verifier = Some("test-pkce-verifier".to_string());

    // Create state with PKCE verifier
    let created = repo
        .create(state.clone(), provider.clone(), verifier.clone(), None)
        .await
        .unwrap();
    assert_eq!(created.pkce_verifier, verifier);
    assert_eq!(created.frontend_callback, None);

    // Get and verify PKCE verifier is preserved
    let retrieved = repo.get_and_delete(&state).await.unwrap().unwrap();
    assert_eq!(retrieved.pkce_verifier, verifier);
}

#[tokio::test]
async fn test_state_replay_protection() {
    let pool = get_test_pool().await;
    let repo = OAuthStateRepository::new(pool.clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let provider = "github".to_string();

    // Create one state
    repo.create(state.clone(), provider, None, None)
        .await
        .unwrap();

    // First get should succeed
    let first = repo.get_and_delete(&state).await.unwrap();
    assert!(first.is_some());

    // Second get should fail (replay protection)
    let second = repo.get_and_delete(&state).await.unwrap();
    assert!(second.is_none());
}

// ============================================
// Response Item Repository workspace scoping (issue nearai/infra#190)
// ============================================

mod response_item_workspace_scoping {
    use crate::common::*;
    use database::PgResponseItemsRepository;
    use services::conversations::models::ConversationId;
    use services::responses::ports::ResponseItemRepositoryTrait;
    use services::workspace::WorkspaceId;
    use uuid::Uuid;

    struct WorkspaceFixture {
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        /// Item IDs (e.g. "msg_<uuid>") in chronological order.
        item_ids: Vec<String>,
    }

    fn parse_conv_uuid(conversation_id: &str) -> Uuid {
        let raw = conversation_id
            .strip_prefix("conv_")
            .unwrap_or(conversation_id);
        Uuid::parse_str(raw).expect("conversation id should contain a UUID")
    }

    /// Creates an org + API key, a conversation, and `item_count` backfilled
    /// items via the public API; returns the raw IDs for repository-level use.
    async fn seed_workspace(server: &axum_test::TestServer, item_count: usize) -> WorkspaceFixture {
        let org = create_org(server).await;
        let workspaces = list_workspaces(server, org.id.clone()).await;
        let workspace = workspaces.first().expect("org should have a workspace");
        let workspace_id =
            WorkspaceId(Uuid::parse_str(&workspace.id).expect("workspace id should be a UUID"));
        let api_key = get_api_key_for_org(server, org.id.clone()).await;

        let create_response = server
            .post("/v1/conversations")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&serde_json::json!({}))
            .await;
        assert_eq!(create_response.status_code(), 201);
        let conversation = create_response.json::<api::models::ConversationObject>();
        let conversation_id = ConversationId(parse_conv_uuid(&conversation.id));

        let mut item_ids = Vec::new();
        for i in 0..item_count {
            let response = server
                .post(format!("/v1/conversations/{}/items", conversation.id).as_str())
                .add_header("Authorization", format!("Bearer {api_key}"))
                .json(&serde_json::json!({
                    "items": [{
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": format!("item {i}")}]
                    }]
                }))
                .await;
            assert_eq!(response.status_code(), 200);
            let created = response.json::<api::models::ConversationItemList>();
            item_ids.push(created.first_id.clone());
        }

        WorkspaceFixture {
            workspace_id,
            conversation_id,
            item_ids,
        }
    }

    fn is_cursor_rejection(error: &anyhow::Error) -> bool {
        error
            .chain()
            .filter_map(|cause| cause.downcast_ref::<services::common::RepositoryError>())
            .any(|e| matches!(e, services::common::RepositoryError::NotFound(_)))
    }

    #[tokio::test]
    async fn test_list_by_conversation_constrained_by_workspace() {
        let (server, database) = setup_test_server_with_database().await;
        let repo = PgResponseItemsRepository::new(database.pool().clone());

        let ws_a = seed_workspace(&server, 3).await;
        let ws_b = seed_workspace(&server, 1).await;

        // Owner sees its own items.
        let own_items = repo
            .list_by_conversation(ws_a.conversation_id, ws_a.workspace_id.clone(), None, 10)
            .await
            .expect("owner listing should succeed");
        assert_eq!(own_items.len(), 3, "owner should see all 3 items");

        // The same conversation queried with a foreign workspace returns
        // nothing, even though the conversation ID is known.
        let foreign_items = repo
            .list_by_conversation(ws_a.conversation_id, ws_b.workspace_id.clone(), None, 10)
            .await
            .expect("foreign listing should not error");
        assert!(
            foreign_items.is_empty(),
            "workspace constraint must exclude foreign conversation items"
        );
    }

    #[tokio::test]
    async fn test_list_by_conversation_rejects_foreign_and_unknown_cursors() {
        let (server, database) = setup_test_server_with_database().await;
        let repo = PgResponseItemsRepository::new(database.pool().clone());

        let ws_a = seed_workspace(&server, 3).await;
        let ws_b = seed_workspace(&server, 1).await;

        // A cursor from the same conversation works.
        let page = repo
            .list_by_conversation(
                ws_a.conversation_id,
                ws_a.workspace_id.clone(),
                Some(ws_a.item_ids[0].clone()),
                10,
            )
            .await
            .expect("own cursor should be accepted");
        assert_eq!(page.len(), 2, "cursor should skip the first item");

        // A cursor that belongs to another workspace's conversation is rejected.
        let foreign_cursor = repo
            .list_by_conversation(
                ws_a.conversation_id,
                ws_a.workspace_id.clone(),
                Some(ws_b.item_ids[0].clone()),
                10,
            )
            .await;
        let err = foreign_cursor.expect_err("foreign cursor must be rejected");
        assert!(
            is_cursor_rejection(&err),
            "foreign cursor should surface as a cursor rejection, got: {err:?}"
        );

        // An unknown cursor is rejected the same way (non-enumerating).
        let unknown_cursor = repo
            .list_by_conversation(
                ws_a.conversation_id,
                ws_a.workspace_id.clone(),
                Some(format!("msg_{}", Uuid::new_v4().simple())),
                10,
            )
            .await;
        let err = unknown_cursor.expect_err("unknown cursor must be rejected");
        assert!(is_cursor_rejection(&err));

        // A cursor from the caller's own OTHER workspace conversation is also
        // rejected: it must belong to this exact conversation.
        let ws_b_own_listing = repo
            .list_by_conversation(
                ws_b.conversation_id,
                ws_b.workspace_id.clone(),
                Some(ws_a.item_ids[0].clone()),
                10,
            )
            .await;
        assert!(
            ws_b_own_listing.is_err(),
            "cross-conversation cursor rejected"
        );
    }

    #[tokio::test]
    async fn test_get_by_id_constrained_by_workspace() {
        let (server, database) = setup_test_server_with_database().await;
        let repo = PgResponseItemsRepository::new(database.pool().clone());

        let ws_a = seed_workspace(&server, 1).await;
        let ws_b = seed_workspace(&server, 1).await;

        let raw_item_id = ws_a.item_ids[0]
            .rsplit('_')
            .next()
            .expect("item id should have a UUID suffix");
        let item_uuid = Uuid::parse_str(raw_item_id).expect("item id should parse");
        let item_id = services::responses::models::ResponseItemId(item_uuid);

        // Owner can fetch the item.
        let own = repo
            .get_by_id(item_id.clone(), ws_a.workspace_id.clone())
            .await
            .expect("owner get_by_id should succeed");
        assert!(own.is_some(), "owner should see its own item");

        // A foreign workspace gets None for the same item ID, identical to a
        // nonexistent item (non-enumerating).
        let foreign = repo
            .get_by_id(item_id, ws_b.workspace_id.clone())
            .await
            .expect("foreign get_by_id should not error");
        assert!(foreign.is_none(), "foreign workspace must not see the item");
    }
}
