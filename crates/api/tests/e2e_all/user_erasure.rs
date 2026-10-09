//! Admin-driven GDPR erasure of a user (spec: user erasure design, rev 3).

use crate::common::*;
use services::auth::ports::OAuthUserInfo;

/// Real signup path: the user gets the production default org and workspace.
async fn signup(
    database: &std::sync::Arc<database::Database>,
    provider: &str,
    provider_user_id: &str,
    email: &str,
) -> (String, uuid::Uuid, String) {
    let mut config = test_config();
    config.auth.mock = false;
    let auth = api::init_auth_services(database.clone(), &config);
    let user = auth
        .auth_service
        .get_or_create_oauth_user(OAuthUserInfo {
            provider: provider.to_string(),
            provider_user_id: provider_user_id.to_string(),
            email: email.to_string(),
            username: format!("u-{provider_user_id}"),
            display_name: Some("Erasure Test".to_string()),
            avatar_url: None,
        })
        .await
        .expect("signup should create the user");
    (format!("rt_{}", user.id.0), user.id.0, email.to_string())
}

async fn new_user(database: &std::sync::Arc<database::Database>) -> (String, uuid::Uuid, String) {
    let u = uuid::Uuid::new_v4();
    signup(
        database,
        "github",
        &format!("gh-{u}"),
        &format!("erase-{u}@test.com"),
    )
    .await
}

async fn count(
    client: &deadpool_postgres::Object,
    sql: &str,
    id: &(dyn tokio_postgres::types::ToSql + Sync),
) -> i64 {
    client.query_one(sql, &[id]).await.unwrap().get(0)
}

#[tokio::test]
async fn login_writes_do_not_touch_an_inactive_user() {
    let (_server, database) = setup_test_server_with_database().await;
    let (_s, user_id, _email) = new_user(&database).await;
    let client = database.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE users SET is_active = false, last_login_at = NULL WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();

    let users = database::repositories::UserRepository::new(database.pool().clone());
    users.update_last_login(user_id).await.unwrap();
    services::auth::UserRepository::update_email(
        &users,
        services::auth::UserId(user_id),
        "leak@test.com".to_string(),
    )
    .await
    .unwrap();

    let row = client
        .query_one(
            "SELECT last_login_at, email FROM users WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    assert!(row
        .get::<_, Option<chrono::DateTime<chrono::Utc>>>("last_login_at")
        .is_none());
    assert_ne!(row.get::<_, String>("email"), "leak@test.com");

    let sessions = database::repositories::SessionRepository::new(database.pool().clone());
    assert!(sessions
        .create(user_id, None, "test-agent".to_string(), 1)
        .await
        .is_err());
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM refresh_tokens WHERE user_id = $1",
            &user_id
        )
        .await,
        0
    );
}

#[tokio::test]
async fn delivery_rows_are_not_written_for_inactive_recipients() {
    let (server, database) = setup_test_server_with_database().await;
    let (_s, user_id, _email) = new_user(&database).await;
    let client = database.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE users SET is_active = false WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    let org: uuid::Uuid = client
        .query_one(
            "SELECT organization_id FROM organization_members WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .unwrap()
        .get(0);
    let model_name = setup_qwen_model(&server).await;
    let model_id: uuid::Uuid = client
        .query_one(
            "SELECT id FROM models WHERE model_name = $1",
            &[&model_name],
        )
        .await
        .unwrap()
        .get(0);

    let repo = database::repositories::AdminCompositeRepository::new(database.pool().clone());
    let record = services::admin::ModelDeprecationDeliveryRecord {
        model_id,
        model_name: model_name.clone(),
        model_display_name: model_name.clone(),
        successor_model_name: model_name.clone(),
        deprecation_date: chrono::Utc::now(),
        recipient_user_id: user_id,
        recipient_email: "x@test.com".to_string(),
        organization_id: org,
        organization_name: "erasure-test-org".to_string(),
        status: services::admin::ModelDeprecationEmailStatus::Sent,
        email_message_id: None,
        email_last_error: None,
        initiated_by_user_id: None,
        initiated_by_user_email: None,
    };
    services::admin::AdminRepository::record_model_deprecation_delivery(&repo, record)
        .await
        .unwrap();

    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM model_deprecation_email_deliveries WHERE recipient_user_id = $1",
            &user_id
        )
        .await,
        0
    );
}

#[tokio::test]
async fn is_user_active_reflects_erasure_state() {
    let (_server, database) = setup_test_server_with_database().await;
    let (_s, user_id, _email) = new_user(&database).await;
    let repo = database::repositories::AdminCompositeRepository::new(database.pool().clone());
    assert!(
        services::admin::AdminRepository::is_user_active(&repo, user_id)
            .await
            .unwrap()
    );
    database
        .pool()
        .get()
        .await
        .unwrap()
        .execute(
            "UPDATE users SET is_active = false WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    assert!(
        !services::admin::AdminRepository::is_user_active(&repo, user_id)
            .await
            .unwrap()
    );
    assert!(
        !services::admin::AdminRepository::is_user_active(&repo, uuid::Uuid::new_v4())
            .await
            .unwrap()
    );
}

async fn personal_org_id(server: &axum_test::TestServer, session: &str) -> String {
    let me = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {session}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
        .json::<serde_json::Value>();
    me["organizations"][0]["id"]
        .as_str()
        .expect("personal org")
        .to_string()
}

async fn preview(server: &axum_test::TestServer, user_id: uuid::Uuid) -> axum_test::TestResponse {
    server
        .post(format!("/v1/admin/users/{user_id}/erasure/preview").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({}))
        .await
}

#[tokio::test]
async fn preview_lists_personal_org_as_erased_and_writes_nothing() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, _email) = new_user(&database).await;
    let org_id = personal_org_id(&server, &session).await;

    let response = preview(&server, user_id).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<serde_json::Value>();
    assert_eq!(body["lifecycle"], "active");
    assert_eq!(body["blockers"].as_array().unwrap().len(), 0);
    assert_eq!(body["erased_organizations"][0]["id"], org_id);
    assert_eq!(body["erased_organizations"][0]["workspaces"], 1);
    assert!(body["log"].is_null());

    let client = database.pool().get().await.unwrap();
    let row = client
        .query_one(
            "SELECT is_active, auth_provider FROM users WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    assert!(row.get::<_, bool>("is_active"), "preview must not write");
    assert_eq!(row.get::<_, String>("auth_provider"), "github");
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM user_erasure_log WHERE user_id = $1",
            &user_id
        )
        .await,
        0
    );
}

#[tokio::test]
async fn preview_unknown_user_is_404() {
    let (server, _database) = setup_test_server_with_database().await;
    assert_eq!(
        preview(&server, uuid::Uuid::new_v4()).await.status_code(),
        404
    );
}
