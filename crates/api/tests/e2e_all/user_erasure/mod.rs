//! Admin-driven GDPR erasure of a user (spec: user erasure design, rev 3).

use crate::common::*;
use services::auth::ports::OAuthUserInfo;

mod admin_lists;
mod blockers;
mod cleanup;
mod concurrency;
mod erase;
mod guards;

/// Real signup path: the user gets the production default org and workspace.
pub(crate) async fn signup(
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

pub(crate) async fn new_user(
    database: &std::sync::Arc<database::Database>,
) -> (String, uuid::Uuid, String) {
    let u = uuid::Uuid::new_v4();
    signup(
        database,
        "github",
        &format!("gh-{u}"),
        &format!("erase-{u}@test.com"),
    )
    .await
}

pub(crate) async fn count(
    client: &deadpool_postgres::Object,
    sql: &str,
    id: &(dyn tokio_postgres::types::ToSql + Sync),
) -> i64 {
    client.query_one(sql, &[id]).await.unwrap().get(0)
}

pub(crate) async fn personal_org_id(server: &axum_test::TestServer, session: &str) -> String {
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

pub(crate) async fn preview(
    server: &axum_test::TestServer,
    user_id: uuid::Uuid,
) -> axum_test::TestResponse {
    server
        .post(format!("/v1/admin/users/{user_id}/erasure/preview").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({}))
        .await
}

pub(crate) async fn add_member(
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

pub(crate) async fn transfer_ownership(
    server: &axum_test::TestServer,
    org_id: &str,
    new_owner: uuid::Uuid,
) {
    let r = server
        .put(format!("/v1/admin/organizations/{org_id}/members/{new_owner}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "role": "owner" }))
        .await;
    assert_eq!(r.status_code(), 200, "{}", r.text());
}

pub(crate) async fn insert_staking_source(
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

pub(crate) async fn insert_admin_token(
    client: &deadpool_postgres::Object,
    user_id: uuid::Uuid,
    active: bool,
) {
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

pub(crate) fn blocker_codes(body: &serde_json::Value) -> Vec<String> {
    body["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["code"].as_str().unwrap().to_string())
        .collect()
}

pub(crate) async fn erase(
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
pub(crate) async fn make_usage(server: &axum_test::TestServer, org_id: &str, api_key: &str) {
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

pub(crate) async fn wait_for_usage(client: &deadpool_postgres::Object, org: uuid::Uuid) -> i64 {
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
