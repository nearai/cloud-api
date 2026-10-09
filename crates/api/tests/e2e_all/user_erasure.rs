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
