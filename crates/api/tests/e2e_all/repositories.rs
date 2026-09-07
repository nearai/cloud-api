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

// Direct repository coverage remains valid during Stage I: these paths do not
// use the retired Conversation or Responses write APIs.
mod database_encryption_at_rest {
    use crate::common::*;
    use database::models::{CreateMcpConnectorRequest, McpAuthType};
    use database::repositories::{FileRepository, McpConnectorRepository};
    use services::files::ports::CreateFileParams;
    use uuid::Uuid;

    #[tokio::test]
    async fn file_repository_encrypts_storage_and_returns_plaintext() {
        let (server, database) = setup_test_server_with_database().await;
        let organization = create_org(&server).await;
        let _api_key = get_api_key_for_org(&server, organization.id.clone()).await;
        let organization_id = Uuid::parse_str(&organization.id).expect("organization UUID");
        let client = database.pool().get().await.expect("database connection");
        let workspace_id: Uuid = client
            .query_one(
                "SELECT id FROM workspaces WHERE organization_id=$1 LIMIT 1",
                &[&organization_id],
            )
            .await
            .expect("workspace")
            .get(0);
        let api_key_id: Uuid = client
            .query_one(
                "SELECT id FROM api_keys WHERE workspace_id=$1 LIMIT 1",
                &[&workspace_id],
            )
            .await
            .expect("API key")
            .get(0);
        drop(client);

        let repository = FileRepository::new(database.pool().clone());
        let created = repository
            .create(CreateFileParams {
                filename: "private.txt".into(),
                bytes: 7,
                content_type: "text/plain".into(),
                purpose: "assistants".into(),
                storage_key: "private/object/key".into(),
                workspace_id,
                uploaded_by_api_key_id: api_key_id,
                expires_at: None,
            })
            .await
            .expect("create encrypted file row");
        assert_eq!(created.filename, "private.txt");
        assert_eq!(created.storage_key, "private/object/key");

        let client = database.pool().get().await.expect("database connection");
        let row = client
            .query_one(
                "SELECT filename,content_type,storage_key FROM files WHERE id=$1",
                &[&created.id],
            )
            .await
            .expect("stored file row");
        for column in ["filename", "content_type", "storage_key"] {
            let stored: String = row.get(column);
            assert!(stored.contains(database::field_encryption::MARKER));
            assert!(!stored.contains("private/object/key"));
        }

        let fetched = repository
            .get_by_id(created.id)
            .await
            .expect("read file")
            .expect("file exists");
        assert_eq!(fetched.filename, "private.txt");
        assert_eq!(fetched.content_type, "text/plain");
        assert_eq!(fetched.storage_key, "private/object/key");

        let configured_key_id = database.pool().encryption_key_id();
        database
            .pool()
            .set_encryption_key_id("wrong-db-key-id".to_string());
        assert!(repository.get_by_id(created.id).await.is_err());
        database.pool().set_encryption_key_id(configured_key_id);

        let client = database.pool().get().await.expect("database connection");
        client
            .execute(
                "UPDATE files SET filename='legacy.txt',content_type='text/legacy',storage_key='legacy/key' WHERE id=$1",
                &[&created.id],
            )
            .await
            .expect("write legacy plaintext row");
        drop(client);
        let legacy = repository
            .get_by_id(created.id)
            .await
            .expect("read legacy file")
            .expect("legacy file exists");
        assert_eq!(legacy.filename, "legacy.txt");
        assert_eq!(legacy.content_type, "text/legacy");
        assert_eq!(legacy.storage_key, "legacy/key");
    }

    #[tokio::test]
    async fn mcp_repository_encrypts_configuration_and_usage_payloads() {
        let (server, database) = setup_test_server_with_database().await;
        let organization = create_org(&server).await;
        let organization_id = Uuid::parse_str(&organization.id).expect("organization UUID");
        let user_id = Uuid::parse_str(MOCK_USER_ID).expect("user UUID");
        let repository = McpConnectorRepository::new(database.pool().clone());

        let connector = repository
            .create(
                organization_id,
                user_id,
                CreateMcpConnectorRequest {
                    name: format!("private-{}", Uuid::new_v4()),
                    description: Some("internal tools".into()),
                    mcp_server_url: "https://internal.example/mcp".into(),
                    auth_type: McpAuthType::Bearer,
                    bearer_token: Some("bearer-secret".into()),
                },
            )
            .await
            .expect("create MCP connector");
        assert_eq!(connector.mcp_server_url, "https://internal.example/mcp");
        assert_eq!(
            connector.auth_config.as_ref().unwrap()["token"],
            "bearer-secret"
        );

        repository
            .log_usage(
                connector.id,
                user_id,
                "tools/call".into(),
                Some(serde_json::json!({"argument":"private request"})),
                Some(serde_json::json!({"result":"private response"})),
                Some(200),
                Some("private error".into()),
                Some(5),
            )
            .await
            .expect("log MCP usage");

        let client = database.pool().get().await.expect("database connection");
        let connector_row = client
            .query_one(
                "SELECT description,mcp_server_url,auth_config FROM mcp_connectors WHERE id=$1",
                &[&connector.id],
            )
            .await
            .expect("stored connector");
        assert!(connector_row
            .get::<_, String>("mcp_server_url")
            .contains(database::field_encryption::MARKER));
        assert_eq!(
            connector_row.get::<_, serde_json::Value>("auth_config")
                [database::field_encryption::MARKER],
            true
        );

        let usage_row = client
            .query_one(
                "SELECT request_payload,response_payload,error_message FROM mcp_connector_usage WHERE connector_id=$1 ORDER BY created_at DESC LIMIT 1",
                &[&connector.id],
            )
            .await
            .expect("stored usage");
        assert_eq!(
            usage_row.get::<_, serde_json::Value>("request_payload")
                [database::field_encryption::MARKER],
            true
        );
        assert_eq!(
            usage_row.get::<_, serde_json::Value>("response_payload")
                [database::field_encryption::MARKER],
            true
        );
        assert!(usage_row
            .get::<_, String>("error_message")
            .contains(database::field_encryption::MARKER));
        drop(client);

        let fetched = repository
            .get_by_id(connector.id)
            .await
            .expect("read connector")
            .expect("connector exists");
        assert_eq!(fetched.mcp_server_url, "https://internal.example/mcp");
        assert_eq!(fetched.auth_config.unwrap()["token"], "bearer-secret");
        let usage = repository
            .get_usage_logs(connector.id, 1)
            .await
            .expect("read usage");
        assert_eq!(
            usage[0].request_payload.as_ref().unwrap()["argument"],
            "private request"
        );
        assert_eq!(
            usage[0].response_payload.as_ref().unwrap()["result"],
            "private response"
        );
        assert_eq!(usage[0].error_message.as_deref(), Some("private error"));

        let client = database.pool().get().await.expect("database connection");
        client
            .execute(
                "UPDATE mcp_connectors SET description='legacy description',mcp_server_url='https://legacy.example/mcp',auth_config=$2 WHERE id=$1",
                &[&connector.id, &serde_json::json!({"token":"legacy token"})],
            )
            .await
            .expect("write legacy connector values");
        drop(client);
        let legacy = repository
            .get_by_id(connector.id)
            .await
            .expect("read legacy connector")
            .expect("legacy connector exists");
        assert_eq!(legacy.description.as_deref(), Some("legacy description"));
        assert_eq!(legacy.mcp_server_url, "https://legacy.example/mcp");
        assert_eq!(legacy.auth_config.unwrap()["token"], "legacy token");
    }
}
