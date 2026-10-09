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

async fn erase(
    server: &axum_test::TestServer,
    user_id: uuid::Uuid,
    confirm_email: &str,
) -> axum_test::TestResponse {
    server
        .post(format!("/v1/admin/users/{user_id}/erasure").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "confirm_email": confirm_email }))
        .await
}

/// Runs one real inference so the org has a usage row (billing ledger) to keep.
async fn make_usage(server: &axum_test::TestServer, org_id: &str, api_key: &str) {
    add_credits_with_type(
        server,
        org_id,
        "grant",
        None,
        10_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;
    let model = setup_qwen_model(server).await;
    let r = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "erasure usage fixture"}],
            "stream": false,
            "max_tokens": 20
        }))
        .await;
    assert_eq!(r.status_code(), 200, "{}", r.text());
}

async fn wait_for_usage(client: &deadpool_postgres::Object, org: uuid::Uuid) -> i64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let n = count(
            client,
            "SELECT COUNT(*) FROM organization_usage_log WHERE organization_id = $1",
            &org,
        )
        .await;
        if n > 0 || std::time::Instant::now() > deadline {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn erase_personal_org_user_removes_content_and_identity_keeps_usage() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, email) = new_user(&database).await;
    let org_id = personal_org_id(&server, &session).await;
    let org: uuid::Uuid = org_id.parse().unwrap();
    let api_key = get_api_key_for_org_with_session(&server, org_id.clone(), &session).await;
    let client = database.pool().get().await.unwrap();

    make_usage(&server, &org_id, &api_key).await;
    let usage_before = wait_for_usage(&client, org).await;
    assert!(usage_before > 0, "fixture must produce a usage row");

    let conv = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(conv.status_code(), 201, "{}", conv.text());
    let file = server
        .post("/v1/files")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(
            axum_test::multipart::MultipartForm::new()
                .add_text("purpose", "user_data")
                .add_part(
                    "file",
                    axum_test::multipart::Part::bytes(b"secret".to_vec())
                        .file_name("notes.txt")
                        .mime_type("text/plain"),
                ),
        )
        .await;
    assert_eq!(file.status_code(), 201, "{}", file.text());

    let response = erase(&server, user_id, &email).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<serde_json::Value>();
    assert_eq!(body["lifecycle"], "erased");
    assert_eq!(body["already_erased"], false);
    assert_eq!(body["erased_organization_ids"][0], org_id);

    let after = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(after.status_code(), 401, "{}", after.text());

    let in_org = "SELECT COUNT(*) FROM {} WHERE workspace_id IN (SELECT id FROM workspaces WHERE organization_id = $1)";
    for table in ["conversations", "responses", "files"] {
        assert_eq!(
            count(&client, &in_org.replace("{}", table), &org).await,
            0,
            "{table}"
        );
    }
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM refresh_tokens WHERE user_id = $1",
            &user_id
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM organization_usage_log WHERE organization_id = $1",
            &org
        )
        .await,
        usage_before,
        "billing ledger survives erasure"
    );

    let u = client
        .query_one(
            "SELECT email, username, display_name, auth_provider, provider_user_id, is_active FROM users WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    assert_eq!(
        u.get::<_, String>("email"),
        format!("erased+{user_id}@erased.invalid")
    );
    assert_eq!(u.get::<_, String>("username"), "erased");
    assert_eq!(u.get::<_, Option<String>>("display_name"), None);
    assert_eq!(u.get::<_, String>("auth_provider"), "erased");
    assert_eq!(u.get::<_, String>("provider_user_id"), user_id.to_string());
    assert!(!u.get::<_, bool>("is_active"));

    let o = client
        .query_one(
            "SELECT name, description, settings, is_active FROM organizations WHERE id = $1",
            &[&org],
        )
        .await
        .unwrap();
    assert_eq!(o.get::<_, String>("name"), format!("erased-{org}"));
    assert_eq!(o.get::<_, Option<String>>("description"), None);
    assert!(o.get::<_, Option<serde_json::Value>>("settings").is_none());
    assert!(!o.get::<_, bool>("is_active"));
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM organization_members WHERE organization_id = $1",
            &org
        )
        .await,
        1
    );
    let prefixes: Vec<Option<String>> = client
        .query(
            "SELECT key_prefix FROM api_keys WHERE workspace_id IN (SELECT id FROM workspaces WHERE organization_id = $1)",
            &[&org],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(prefixes.iter().all(|p| p.as_deref() == Some("sk-****")));

    let log = client
        .query_one(
            "SELECT erased_organization_count FROM user_erasure_log WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    assert_eq!(log.get::<_, i32>("erased_organization_count"), 1);
}

#[tokio::test]
async fn erased_user_can_sign_up_again_with_same_identity() {
    let (server, database) = setup_test_server_with_database().await;
    let u = uuid::Uuid::new_v4();
    let provider_id = format!("gh-{u}");
    let email = format!("again-{u}@test.com");
    let (_s, first, _) = signup(&database, "github", &provider_id, &email).await;
    assert_eq!(erase(&server, first, &email).await.status_code(), 200);
    let (_s2, second, _) = signup(&database, "github", &provider_id, &email).await;
    assert_ne!(first, second, "re-signup must create a fresh account");
}

#[tokio::test]
async fn erase_rejects_mismatched_email_and_accepts_case_and_whitespace_variants() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    let wrong = erase(&server, user_id, "someone-else@test.com").await;
    assert_eq!(wrong.status_code(), 422, "{}", wrong.text());
    assert_eq!(
        wrong.json::<serde_json::Value>()["error"]["type"],
        "confirm_email_mismatch"
    );
    assert_eq!(
        erase(&server, user_id, &format!("  {}  ", email.to_uppercase()))
            .await
            .status_code(),
        200
    );
}

#[tokio::test]
async fn erase_again_reports_already_erased() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    let first = erase(&server, user_id, &email)
        .await
        .json::<serde_json::Value>();
    let again = erase(&server, user_id, "anything").await;
    assert_eq!(again.status_code(), 200, "{}", again.text());
    let again = again.json::<serde_json::Value>();
    assert_eq!(again["already_erased"], true);
    assert_eq!(
        again["erased_organization_ids"],
        first["erased_organization_ids"]
    );
}

#[tokio::test]
async fn erase_blocked_returns_structured_blockers_and_changes_nothing() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, email) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;

    let response = erase(&server, owner, &email).await;
    assert_eq!(response.status_code(), 409, "{}", response.text());
    let body = response.json::<serde_json::Value>();
    assert_eq!(body["blockers"][0]["code"], "sole_owner_of_shared_org");
    let row = database
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT is_active, email FROM users WHERE id = $1",
            &[&owner],
        )
        .await
        .unwrap();
    assert!(row.get::<_, bool>("is_active"));
    assert_eq!(row.get::<_, String>("email"), email);
}

#[tokio::test]
async fn erase_after_ownership_transfer_keeps_team_org_and_keys() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, owner_email) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;
    let team_key = get_api_key_for_org_with_session(&server, team.id.clone(), &session).await;
    transfer_ownership(&server, &team.id, teammate).await;

    assert_eq!(erase(&server, owner, &owner_email).await.status_code(), 200);

    let client = database.pool().get().await.unwrap();
    let team_uuid: uuid::Uuid = team.id.parse().unwrap();
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM organization_members WHERE organization_id = $1",
            &team_uuid
        )
        .await,
        1
    );
    let ok = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {team_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(
        ok.status_code(),
        201,
        "a key created in a retained org belongs to the org: {}",
        ok.text()
    );
}

#[tokio::test]
async fn erase_renames_retained_org_still_named_after_user_email() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, owner_email) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let personal = personal_org_id(&server, &session).await;
    add_member(&database, &personal, teammate, "member").await;
    transfer_ownership(&server, &personal, teammate).await;
    let client = database.pool().get().await.unwrap();
    let org: uuid::Uuid = personal.parse().unwrap();
    let old: String = client
        .query_one("SELECT name FROM organizations WHERE id = $1", &[&org])
        .await
        .unwrap()
        .get(0);
    assert!(old.starts_with(&format!("{}-org-", owner_email.split('@').next().unwrap())));

    assert_eq!(erase(&server, owner, &owner_email).await.status_code(), 200);

    let name: String = client
        .query_one("SELECT name FROM organizations WHERE id = $1", &[&org])
        .await
        .unwrap()
        .get(0);
    assert_eq!(name, format!("org-{org}"));
    let desc: Option<String> = client
        .query_one(
            "SELECT description FROM workspaces WHERE organization_id = $1 AND name = 'default'",
            &[&org],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        desc.as_deref(),
        Some(format!("Default workspace for org-{org}").as_str())
    );
}

#[tokio::test]
async fn erase_leaves_owner_deleted_shared_org_without_blocking() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, email) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;
    let team_uuid: uuid::Uuid = team.id.parse().unwrap();
    let client = database.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&team_uuid],
        )
        .await
        .unwrap();

    assert_eq!(erase(&server, owner, &email).await.status_code(), 200);
    let left: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM organization_members WHERE organization_id = $1 AND user_id = $2",
            &[&team_uuid, &owner],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(left, 0);
}

#[tokio::test]
async fn erase_user_with_no_memberships() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    database
        .pool()
        .get()
        .await
        .unwrap()
        .execute(
            "DELETE FROM organization_members WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    let r = erase(&server, user_id, &email).await;
    assert_eq!(r.status_code(), 200, "{}", r.text());
    assert_eq!(
        r.json::<serde_json::Value>()["erased_organization_ids"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn retry_sweeps_content_written_after_erasure() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, email) = new_user(&database).await;
    let org: uuid::Uuid = personal_org_id(&server, &session).await.parse().unwrap();
    // Create a key before erasure so a key row exists for the late insert below.
    get_api_key_for_org_with_session(&server, org.to_string(), &session).await;
    assert_eq!(erase(&server, user_id, &email).await.status_code(), 200);

    // Simulate an in-flight stream landing after commit.
    let client = database.pool().get().await.unwrap();
    let r = client.query_one(
        "SELECT w.id, k.id FROM workspaces w JOIN api_keys k ON k.workspace_id = w.id WHERE w.organization_id = $1 LIMIT 1",
        &[&org],
    ).await.unwrap();
    let (ws, key): (uuid::Uuid, uuid::Uuid) = (r.get(0), r.get(1));
    client.execute(
        "INSERT INTO conversations (id, workspace_id, api_key_id, metadata) VALUES ($1, $2, $3, '{}'::jsonb)",
        &[&uuid::Uuid::new_v4(), &ws, &key],
    ).await.unwrap();

    let again = erase(&server, user_id, "x")
        .await
        .json::<serde_json::Value>();
    assert_eq!(again["already_erased"], true);
    assert_eq!(count(&client, "SELECT COUNT(*) FROM conversations WHERE workspace_id IN (SELECT id FROM workspaces WHERE organization_id = $1)", &org).await, 0);
}

#[tokio::test]
async fn erase_scrubs_denormalized_emails_tokens_votes_and_invitations() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, email) = new_user(&database).await;
    let org: uuid::Uuid = personal_org_id(&server, &session).await.parse().unwrap();
    let client = database.pool().get().await.unwrap();
    insert_admin_token(&client, user_id, false).await;
    client.execute(
        "INSERT INTO organization_limits_history (organization_id, spend_limit, effective_from, changed_by_user_id, changed_by_user_email, credit_type, source, currency) \
         VALUES ($1, 0, NOW(), $2, $3, 'grant', 'erasure-test', 'USD')",
        &[&org, &user_id, &email],
    ).await.unwrap();
    let outsider = format!("outsider-{}@test.com", uuid::Uuid::new_v4());
    client.execute(
        "INSERT INTO organization_invitations (organization_id, email, role, invited_by_user_id, token, expires_at) \
         VALUES ($1, $2, 'member', $3, $4, NOW() + INTERVAL '7 days')",
        &[&org, &outsider, &user_id, &uuid::Uuid::new_v4().to_string()],
    ).await.unwrap();
    let target: uuid::Uuid = client.query_one(
        "INSERT INTO feature_request_targets (kind, key, title) VALUES ('feature', $1, 'my private idea') RETURNING id",
        &[&format!("erase-{}", uuid::Uuid::new_v4())],
    ).await.unwrap().get(0);
    client
        .execute(
            "INSERT INTO feature_request_votes (target_id, user_id) VALUES ($1, $2)",
            &[&target, &user_id],
        )
        .await
        .unwrap();

    assert_eq!(erase(&server, user_id, &email).await.status_code(), 200);

    let tomb = format!("erased+{user_id}@erased.invalid");
    let limits_email: Option<String> = client
        .query_one("SELECT changed_by_user_email FROM organization_limits_history WHERE changed_by_user_id = $1 LIMIT 1", &[&user_id])
        .await.unwrap().get(0);
    assert_eq!(limits_email.as_deref(), Some(tomb.as_str()));
    let token = client.query_one(
        "SELECT name, creation_reason, revocation_reason, user_agent FROM admin_access_token WHERE created_by_user_id = $1", &[&user_id],
    ).await.unwrap();
    assert_eq!(token.get::<_, String>("name"), "erased");
    assert_eq!(
        token
            .get::<_, Option<String>>("revocation_reason")
            .as_deref(),
        Some("erased")
    );
    assert_eq!(token.get::<_, Option<String>>("user_agent"), None);
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM organization_invitations WHERE organization_id = $1",
            &org
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM feature_request_targets WHERE id = $1",
            &target
        )
        .await,
        0
    );
}

#[tokio::test]
async fn erase_disconnects_staking_source_of_erased_org() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, email) = new_user(&database).await;
    let org: uuid::Uuid = personal_org_id(&server, &session).await.parse().unwrap();
    let client = database.pool().get().await.unwrap();
    insert_staking_source(&client, org, user_id, "[]", 1).await;
    assert_eq!(erase(&server, user_id, &email).await.status_code(), 200);
    let status: String = client
        .query_one(
            "SELECT status FROM organization_staking_farm_sources WHERE organization_id = $1",
            &[&org],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(status, "disconnected");
}

#[tokio::test]
async fn erase_blocked_by_active_admin_token() {
    let (server, database) = setup_test_server_with_database().await;
    let (_s, user_id, email) = new_user(&database).await;
    insert_admin_token(&database.pool().get().await.unwrap(), user_id, true).await;
    let r = erase(&server, user_id, &email).await;
    assert_eq!(r.status_code(), 409, "{}", r.text());
    assert_eq!(
        r.json::<serde_json::Value>()["blockers"][0]["code"],
        "active_admin_tokens"
    );
}

#[tokio::test]
async fn near_user_can_log_in_again_after_erasure() {
    let (server, database) = setup_test_server_with_database().await;
    let account = format!("erase-{}.testnet", uuid::Uuid::new_v4().simple());
    let email = format!("{account}@near");
    let (_s, first, _) = signup(&database, "near", &account, &email).await;
    assert_eq!(erase(&server, first, &email).await.status_code(), 200);
    let (_s2, second, _) = signup(&database, "near", &account, &email).await;
    assert_ne!(first, second);
}

#[tokio::test]
async fn admin_user_list_shows_lifecycle() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    assert_eq!(erase(&server, user_id, &email).await.status_code(), 200);
    let users = server
        .get(format!("/v1/admin/users?search={user_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await
        .json::<serde_json::Value>();
    let user = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == user_id.to_string())
        .unwrap_or_else(|| panic!("erased user missing from list: {users}"));
    assert_eq!(user["lifecycle"], "erased");
}

#[tokio::test]
async fn admin_org_list_lifecycle_filter_returns_only_matching_rows() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    assert_eq!(erase(&server, user_id, &email).await.status_code(), 200);
    for filter in ["erased", "deleted", "active"] {
        let body = server
            .get(format!("/v1/admin/organizations?lifecycle={filter}&limit=50&offset=0").as_str())
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .await
            .json::<serde_json::Value>();
        // Property over whatever the first page holds; no completeness or ordering claim.
        assert!(
            body["organizations"]
                .as_array()
                .unwrap()
                .iter()
                .all(|o| o["lifecycle"] == filter),
            "{filter}: {body}"
        );
    }
}
