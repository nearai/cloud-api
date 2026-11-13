//! Contact email columns of NEAR wallet users: wallet logins keep a stored
//! address, `GET /v1/users/me` reports it, and notification recipients prefer it.

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;
use chrono::{DateTime, Utc};
use database::Database;
use services::admin::AdminRepository;
use services::auth::ports::OAuthUserInfo;
use services::auth::AuthServiceTrait;
use services::email::near_placeholder_email;
use std::sync::Arc;
use uuid::Uuid;

/// Stores a verified contact email directly: the routes that wrote it are no
/// longer served here, but the columns are still read.
async fn set_verified_contact_email(
    client: &deadpool_postgres::Object,
    user_id: Uuid,
    address: &str,
) {
    client
        .execute(
            "UPDATE users SET contact_email = $2, contact_email_verified_at = NOW(), \
             email_opt_in_at = NOW() WHERE id = $1",
            &[&user_id, &address],
        )
        .await
        .unwrap();
}

/// Real signup path: the user gets the production default org and workspace.
async fn signup(
    database: &Arc<Database>,
    provider: &str,
    provider_user_id: &str,
    email: &str,
) -> (String, Uuid, String) {
    let mut config = test_config();
    config.auth.mock = false;
    let auth = api::init_auth_services(database.clone(), &config);
    let user = auth
        .auth_service
        .get_or_create_oauth_user(OAuthUserInfo {
            email_verified: true,
            email_authoritative: true,
            provider: provider.to_string(),
            provider_user_id: provider_user_id.to_string(),
            email: email.to_string(),
            username: format!("u-{provider_user_id}"),
            display_name: Some("Contact Email Test".to_string()),
            avatar_url: None,
        })
        .await
        .expect("signup should create the user");
    (format!("rt_{}", user.id.0), user.id.0, email.to_string())
}

async fn new_user(database: &Arc<Database>) -> (String, Uuid, String) {
    let u = Uuid::new_v4();
    signup(
        database,
        "github",
        &format!("gh-{u}"),
        &format!("contact-{u}@test.com"),
    )
    .await
}

async fn personal_org_id(server: &axum_test::TestServer, session: &str) -> String {
    me(server, session).await["organizations"][0]["id"]
        .as_str()
        .expect("personal org")
        .to_string()
}

/// The real (non-mock) auth service, which holds the login code under test.
fn real_auth(database: &Arc<Database>) -> Arc<dyn AuthServiceTrait> {
    let mut config = test_config();
    config.auth.mock = false;
    api::init_auth_services(database.clone(), &config).auth_service
}

/// What a NEAR wallet login does once the signature is verified: the same
/// `OAuthUserInfo` that `NearAuthService` builds, through the real auth service.
/// Returns the session accepted by the mock-auth e2e server and the user id.
async fn near_login(auth: &Arc<dyn AuthServiceTrait>, account_id: &str) -> (String, Uuid) {
    let user = auth
        .get_or_create_oauth_user(OAuthUserInfo {
            email_verified: true,
            email_authoritative: true,
            provider: "near".to_string(),
            provider_user_id: account_id.to_string(),
            email: near_placeholder_email(account_id),
            username: account_id.to_string(),
            display_name: Some(account_id.to_string()),
            avatar_url: None,
        })
        .await
        .expect("wallet login should succeed");
    (format!("rt_{}", user.id.0), user.id.0)
}

fn new_account_id() -> String {
    format!("ce-{}.testnet", Uuid::new_v4().simple())
}

fn new_address() -> String {
    format!("ce-{}@example.com", Uuid::new_v4().simple())
}

async fn me(server: &axum_test::TestServer, session: &str) -> serde_json::Value {
    let response = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {session}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response.json()
}

type ContactColumns = (
    String,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

/// `(email, contact_email, contact_email_verified_at, email_opt_in_at)`.
async fn contact_columns(client: &deadpool_postgres::Object, user_id: Uuid) -> ContactColumns {
    let row = client
        .query_one(
            "SELECT email, contact_email, contact_email_verified_at, email_opt_in_at \
             FROM users WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1), row.get(2), row.get(3))
}

fn minimal_model_upsert(extra_aliases: &[&str]) -> serde_json::Value {
    let mut v = serde_json::json!({
        "inputCostPerToken":  { "amount": 1_000_000, "currency": "USD" },
        "outputCostPerToken": { "amount": 2_000_000, "currency": "USD" },
        "modelDisplayName":   "Deprecation Test Model",
        "modelDescription":   "Synthetic model for deprecation e2e",
        "contextLength":      4096,
        "maxOutputLength": 1024,
        "verifiable":         false,
        "isActive":           true,
    });
    if !extra_aliases.is_empty() {
        v["aliases"] = serde_json::json!(extra_aliases);
    }
    v
}

async fn create_model(
    server: &axum_test::TestServer,
    name: &str,
    aliases: &[&str],
) -> api::models::ModelWithPricing {
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        name.to_string(),
        serde_json::from_value(minimal_model_upsert(aliases)).unwrap(),
    );
    let updated = seed_models(server, batch, get_session_id()).await;
    updated.into_iter().next().expect("Model should be created")
}

async fn seed_recent_usage(
    database: &database::Database,
    organization_id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    api_key_id: uuid::Uuid,
    model_name: &str,
) {
    let client = database.pool().get().await.unwrap();
    let model_id: uuid::Uuid = client
        .query_one(
            "SELECT id FROM models WHERE model_name = $1",
            &[&model_name],
        )
        .await
        .unwrap()
        .get(0);
    client
        .execute(
            r#"
            INSERT INTO organization_usage_log (
                id, organization_id, workspace_id, api_key_id,
                model_id, model_name, input_tokens, output_tokens,
                total_tokens, input_cost, output_cost, total_cost,
                inference_type, created_at
            ) VALUES ($1, $2, $3, $4, $5, $6, 10, 10, 20, 1, 1, 2,
                      'chat_completion', NOW())
            "#,
            &[
                &uuid::Uuid::new_v4(),
                &organization_id,
                &workspace_id,
                &api_key_id,
                &model_id,
                &model_name,
            ],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn contact_email_is_unchanged_by_later_wallet_logins() {
    let (server, database) = setup_test_server_with_database().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();
    let account_id = new_account_id();
    let address = new_address();
    let (session, user_id) = near_login(&auth, &account_id).await;
    set_verified_contact_email(&client, user_id, &address).await;
    let confirmed = contact_columns(&client, user_id).await;
    assert_eq!(confirmed.1.as_deref(), Some(address.as_str()));

    // The same wallet signs in again, twice.
    for _ in 0..2 {
        let (_, same_user) = near_login(&auth, &account_id).await;
        assert_eq!(same_user, user_id);
        assert_eq!(contact_columns(&client, user_id).await, confirmed);
    }

    // A login that has to rewrite `users.email` and the profile (the stored row
    // drifted from what the provider reports) still leaves the three columns alone.
    client
        .execute(
            "UPDATE users SET email = $2, display_name = 'Renamed' WHERE id = $1",
            &[&user_id, &format!("drifted-{}@example.com", Uuid::new_v4())],
        )
        .await
        .unwrap();
    let (_, same_user) = near_login(&auth, &account_id).await;
    assert_eq!(same_user, user_id);
    assert_eq!(contact_columns(&client, user_id).await, confirmed);

    let after = me(&server, &session).await;
    assert_eq!(after["email"], near_placeholder_email(&account_id));
    assert_eq!(after["display_name"], account_id);
    assert_eq!(after["contact_email"], address, "{after}");
    assert_eq!(after["contact_email_verified"], true, "{after}");
    assert_eq!(after["needs_contact_email"], false, "{after}");
}

#[tokio::test]
async fn notification_recipients_skip_placeholder_users_and_use_the_contact_email() {
    let (server, database) = setup_test_server_with_database().await;
    let auth = real_auth(&database);
    let old = format!("test-contact-email-old-{}", Uuid::new_v4());
    create_model(&server, &old, &[]).await;

    // Two organizations used the model: one owned by a wallet user, one by a
    // user who signs in with an email.
    let account_id = new_account_id();
    let (wallet_session, wallet_user) = near_login(&auth, &account_id).await;
    let (email_session, email_user, login_address) = new_user(&database).await;
    for session in [&wallet_session, &email_session] {
        let org_id = personal_org_id(&server, session).await;
        let workspaces = list_workspaces_with_session(&server, org_id.clone(), session).await;
        let workspace = workspaces.first().unwrap();
        let api_key = create_api_key_in_workspace_with_session(
            &server,
            workspace.id.clone(),
            "contact-email".to_string(),
            session,
        )
        .await;
        seed_recent_usage(
            &database,
            Uuid::parse_str(&org_id).unwrap(),
            Uuid::parse_str(&workspace.id).unwrap(),
            Uuid::parse_str(&api_key.id).unwrap(),
            &old,
        )
        .await;
    }

    let repository = database::repositories::AdminCompositeRepository::new(database.pool().clone());
    let since = Utc::now() - chrono::Duration::days(30);
    let recipients = || async {
        let deprecation: Vec<(Uuid, String)> = repository
            .list_model_deprecation_recipients(&old, since)
            .await
            .unwrap()
            .into_iter()
            .map(|recipient| (recipient.user_id, recipient.email))
            .collect();
        let pricing: Vec<(Uuid, String)> = repository
            .list_pricing_change_recipients(std::slice::from_ref(&old), since)
            .await
            .unwrap()
            .into_iter()
            .map(|recipient| (recipient.user_id, recipient.email))
            .collect();
        (deprecation, pricing)
    };

    // The wallet user has only a placeholder address: not a recipient.
    let only_email_user = vec![(email_user, login_address.clone())];
    assert_eq!(
        recipients().await,
        (only_email_user.clone(), only_email_user)
    );

    // Once the contact email is verified, it is the address used.
    let address = new_address();
    set_verified_contact_email(&database.pool().get().await.unwrap(), wallet_user, &address).await;
    let (mut deprecation, mut pricing) = recipients().await;
    let mut expected = vec![(email_user, login_address), (wallet_user, address)];
    deprecation.sort();
    pricing.sort();
    expected.sort();
    assert_eq!(deprecation, expected);
    assert_eq!(pricing, expected);
}

#[test]
fn openapi_documents_the_contact_email_fields() {
    use utoipa::OpenApi;

    let spec = serde_json::to_value(api::openapi::ApiDoc::openapi()).unwrap();
    let schemas = &spec["components"]["schemas"];
    for (schema, fields) in [
        (
            "UserResponse",
            [
                "contact_email",
                "contact_email_verified",
                "needs_contact_email",
            ],
        ),
        (
            "AdminUserResponse",
            [
                "contact_email",
                "contact_email_verified_at",
                "email_opt_in_at",
            ],
        ),
    ] {
        for field in fields {
            assert!(
                schemas[schema]["properties"][field].is_object(),
                "{schema}.{field} is missing from the OpenAPI document"
            );
        }
    }
}
