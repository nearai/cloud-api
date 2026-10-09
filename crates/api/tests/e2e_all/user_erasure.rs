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

async fn add_member(
    database: &std::sync::Arc<database::Database>,
    org_id: &str,
    user_id: uuid::Uuid,
    role: &str,
) {
    let org: uuid::Uuid = org_id.parse().unwrap();
    database
        .pool()
        .get()
        .await
        .unwrap()
        .execute(
            "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, $3)",
            &[&org, &user_id, &role],
        )
        .await
        .unwrap();
}

async fn transfer_ownership(server: &axum_test::TestServer, org_id: &str, new_owner: uuid::Uuid) {
    let r = server
        .put(format!("/v1/admin/organizations/{org_id}/members/{new_owner}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "role": "owner" }))
        .await;
    assert_eq!(r.status_code(), 200, "{}", r.text());
}

async fn insert_staking_source(
    client: &deadpool_postgres::Object,
    org: uuid::Uuid,
    user_id: uuid::Uuid,
    positions_json: &str,
    synced_hours_ago: i32,
) {
    let positions: serde_json::Value = serde_json::from_str(positions_json).unwrap();
    client
        .execute(
            "INSERT INTO organization_staking_farm_sources \
               (organization_id, near_account_id, network_id, contract_id, farm_product_id, \
                credit_nano_usd_per_reward_unit, status, sync_status, \
                active_positions, created_by_user_id, last_synced_at) \
             VALUES ($1, $2, 'testnet', 'farm.testnet', 'erasure-farm', 1, 'active', 'synced', \
                     $3, $4, NOW() - make_interval(hours => $5))",
            &[
                &org,
                &format!("erase-{}.testnet", uuid::Uuid::new_v4().simple()),
                &positions,
                &user_id,
                &synced_hours_ago,
            ],
        )
        .await
        .unwrap();
}

async fn insert_admin_token(client: &deadpool_postgres::Object, user_id: uuid::Uuid, active: bool) {
    client
        .execute(
            "INSERT INTO admin_access_token \
               (token_hash, created_by_user_id, name, creation_reason, expires_at, is_active, \
                revoked_at, revocation_reason, user_agent) \
             VALUES ($1, $2, 'Erasure Test Token', 'test', NOW() + INTERVAL '1 day', $3, \
                     CASE WHEN $3 THEN NULL ELSE NOW() END, \
                     CASE WHEN $3 THEN NULL ELSE 'rotated' END, 'test-agent')",
            &[
                &format!("{:064x}", uuid::Uuid::new_v4().as_u128()),
                &user_id,
                &active,
            ],
        )
        .await
        .unwrap();
}

fn blocker_codes(body: &serde_json::Value) -> Vec<String> {
    body["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["code"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn preview_blocks_sole_owner_of_active_shared_org() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, _) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;

    let body = preview(&server, owner).await.json::<serde_json::Value>();
    assert_eq!(blocker_codes(&body), vec!["sole_owner_of_shared_org"]);
    assert_eq!(body["blockers"][0]["organization_id"], team.id);
}

#[tokio::test]
async fn preview_allows_owner_after_ownership_transfer() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, _) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;
    transfer_ownership(&server, &team.id, teammate).await;

    let body = preview(&server, owner).await.json::<serde_json::Value>();
    assert!(blocker_codes(&body).is_empty(), "{body}");
    assert!(body["retained_organizations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|o| o["id"] == team.id));
}

#[tokio::test]
async fn preview_does_not_block_on_owner_deleted_shared_org() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, _) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;
    let team_uuid: uuid::Uuid = team.id.parse().unwrap();
    database
        .pool()
        .get()
        .await
        .unwrap()
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&team_uuid],
        )
        .await
        .unwrap();

    let body = preview(&server, owner).await.json::<serde_json::Value>();
    assert!(blocker_codes(&body).is_empty(), "{body}");
}

#[tokio::test]
async fn preview_blocks_active_or_stale_staking() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, _) = new_user(&database).await;
    let org: uuid::Uuid = personal_org_id(&server, &session).await.parse().unwrap();
    let client = database.pool().get().await.unwrap();
    insert_staking_source(&client, org, user_id, r#"[{"amount":"10"}]"#, 1).await;
    assert_eq!(
        blocker_codes(&preview(&server, user_id).await.json()),
        vec!["staking_active"]
    );

    client
        .execute(
            "UPDATE organization_staking_farm_sources SET active_positions = '[]'::jsonb, \
             last_synced_at = NOW() - INTERVAL '48 hours' WHERE organization_id = $1",
            &[&org],
        )
        .await
        .unwrap();
    assert_eq!(
        blocker_codes(&preview(&server, user_id).await.json()),
        vec!["staking_active"],
        "stale sync must still block"
    );

    client
        .execute(
            "UPDATE organization_staking_farm_sources SET last_synced_at = NOW() \
             WHERE organization_id = $1",
            &[&org],
        )
        .await
        .unwrap();
    assert!(blocker_codes(&preview(&server, user_id).await.json()).is_empty());
}

#[tokio::test]
async fn preview_blocks_active_admin_tokens_only() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, _) = new_user(&database).await;
    let client = database.pool().get().await.unwrap();
    insert_admin_token(&client, user_id, false).await;
    assert!(
        blocker_codes(&preview(&server, user_id).await.json()).is_empty(),
        "revoked tokens do not block"
    );
    insert_admin_token(&client, user_id, true).await;
    let body = preview(&server, user_id).await.json::<serde_json::Value>();
    assert_eq!(blocker_codes(&body), vec!["active_admin_tokens"]);
    assert_eq!(body["blockers"][0]["count"], 1);
}
