//! Writes and lookups that must ignore a deactivated (erased) user. The erasure routes are
//! gone; these guards live in kept repositories, so users are deactivated with SQL.

use crate::common::*;
use services::auth::ports::OAuthUserInfo;

/// Real signup path (as OAuth login does): the user gets a default org and workspace.
async fn new_user(database: &std::sync::Arc<database::Database>) -> (uuid::Uuid, uuid::Uuid) {
    let mut config = test_config();
    config.auth.mock = false;
    let auth = api::init_auth_services(database.clone(), &config);
    let unique = uuid::Uuid::new_v4();
    let user = auth
        .auth_service
        .get_or_create_oauth_user(OAuthUserInfo {
            email_verified: true,
            email_authoritative: true,
            provider: "github".to_string(),
            provider_user_id: format!("gh-{unique}"),
            email: format!("guard-{unique}@test.com"),
            username: format!("guard-{unique}"),
            display_name: Some("Guard Test".to_string()),
            avatar_url: None,
        })
        .await
        .expect("signup should create the user");
    let client = database.pool().get().await.unwrap();
    let org: uuid::Uuid = client
        .query_one(
            "SELECT organization_id FROM organization_members WHERE user_id = $1",
            &[&user.id.0],
        )
        .await
        .unwrap()
        .get(0);
    (user.id.0, org)
}

async fn deactivate(database: &std::sync::Arc<database::Database>, user_id: uuid::Uuid) {
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
}

async fn count(
    database: &std::sync::Arc<database::Database>,
    sql: &str,
    id: &(dyn tokio_postgres::types::ToSql + Sync),
) -> i64 {
    database
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(sql, &[id])
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn login_writes_do_not_touch_an_inactive_user() {
    let (_server, database) = setup_test_server_with_database().await;
    let (user_id, _org) = new_user(&database).await;
    deactivate(&database, user_id).await;
    let client = database.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE users SET last_login_at = NULL WHERE id = $1",
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
            &database,
            "SELECT COUNT(*) FROM refresh_tokens WHERE user_id = $1",
            &user_id
        )
        .await,
        0
    );
}

#[tokio::test]
async fn user_lookups_ignore_an_inactive_user() {
    let (_server, database) = setup_test_server_with_database().await;
    let (user_id, _org) = new_user(&database).await;
    let users = database::repositories::UserRepository::new(database.pool().clone());
    let user = users
        .get_by_id(user_id)
        .await
        .unwrap()
        .expect("active user");
    assert_eq!(
        users
            .get_by_provider("github", &user.provider_user_id)
            .await
            .unwrap()
            .map(|u| u.id),
        Some(user_id),
        "positive control"
    );

    deactivate(&database, user_id).await;
    assert!(users.get_by_id(user_id).await.unwrap().is_none());
    assert!(users.get_by_email(&user.email).await.unwrap().is_none());
    assert!(users
        .get_by_provider("github", &user.provider_user_id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn is_user_active_reflects_deactivation() {
    let (_server, database) = setup_test_server_with_database().await;
    let (user_id, _org) = new_user(&database).await;
    let repo = database::repositories::AdminCompositeRepository::new(database.pool().clone());
    assert!(
        services::admin::AdminRepository::is_user_active(&repo, user_id)
            .await
            .unwrap()
    );
    deactivate(&database, user_id).await;
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

#[tokio::test]
async fn deprecation_delivery_rows_are_not_written_for_inactive_recipients() {
    let (server, database) = setup_test_server_with_database().await;
    let (inactive_user, inactive_org) = new_user(&database).await;
    let (active_user, active_org) = new_user(&database).await;
    deactivate(&database, inactive_user).await;
    let model_name = setup_qwen_model(&server).await;
    let model_id: uuid::Uuid = database
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT id FROM models WHERE model_name = $1",
            &[&model_name],
        )
        .await
        .unwrap()
        .get(0);

    let repo = database::repositories::AdminCompositeRepository::new(database.pool().clone());
    for (user, org) in [(inactive_user, inactive_org), (active_user, active_org)] {
        let record = services::admin::ModelDeprecationDeliveryRecord {
            model_id,
            model_name: model_name.clone(),
            model_display_name: model_name.clone(),
            successor_model_name: model_name.clone(),
            deprecation_date: chrono::Utc::now(),
            recipient_user_id: user,
            recipient_email: "x@test.com".to_string(),
            organization_id: org,
            organization_name: "guard-test-org".to_string(),
            status: services::admin::ModelDeprecationEmailStatus::Sent,
            email_message_id: None,
            email_last_error: None,
            initiated_by_user_id: None,
            initiated_by_user_email: None,
        };
        services::admin::AdminRepository::record_model_deprecation_delivery(&repo, record)
            .await
            .unwrap();
    }

    let sql =
        "SELECT COUNT(*) FROM model_deprecation_email_deliveries WHERE recipient_user_id = $1";
    assert_eq!(count(&database, sql, &inactive_user).await, 0);
    assert_eq!(
        count(&database, sql, &active_user).await,
        1,
        "positive control: an active recipient gets a row"
    );
}

#[tokio::test]
async fn pricing_change_delivery_rows_are_not_written_for_inactive_recipients() {
    let (_server, database) = setup_test_server_with_database().await;
    let (inactive_user, inactive_org) = new_user(&database).await;
    let (active_user, active_org) = new_user(&database).await;
    deactivate(&database, inactive_user).await;

    let repo = database::repositories::AdminCompositeRepository::new(database.pool().clone());
    let batch_id = uuid::Uuid::new_v4();
    for (user, org) in [(inactive_user, inactive_org), (active_user, active_org)] {
        let record = services::admin::PricingChangeDeliveryRecord {
            batch_id,
            recipient_user_id: user,
            recipient_email: "x@test.com".to_string(),
            organization_id: org,
            organization_name: "guard-test-org".to_string(),
            model_names: vec![format!("guard-test-model-{batch_id}")],
            status: services::admin::ModelDeprecationEmailStatus::Sent,
            email_message_id: None,
            email_last_error: None,
            initiated_by_user_id: None,
            initiated_by_user_email: None,
        };
        services::admin::AdminRepository::record_pricing_change_delivery(&repo, record)
            .await
            .unwrap();
    }

    let sql =
        "SELECT COUNT(*) FROM model_pricing_change_email_deliveries WHERE recipient_user_id = $1";
    assert_eq!(count(&database, sql, &inactive_user).await, 0);
    assert_eq!(
        count(&database, sql, &active_user).await,
        1,
        "positive control: an active recipient gets a row"
    );
}
