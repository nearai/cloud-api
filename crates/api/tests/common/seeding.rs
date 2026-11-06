//! Direct database seeding for e2e setup.
//!
//! Setup state (organizations, workspaces, API keys, credits, platform services and
//! catalog models) is written to the shared test database with plain SQL, so tests that
//! are not about those management routes do not depend on them. The SQL mirrors what the
//! management handlers wrote: same columns, same defaults, same ledger rows.
//!
//! The pool is created once per process on first use. Seeded users must already exist in
//! `users` (see `setup_unique_test_session`); the mock auth resolves `rt_<uuid>` to that
//! user and anything else to `MOCK_USER_ID`.

use super::db_setup::{self, MOCK_USER_ID};
use api::models::{
    ApiKeyResponse, DecimalPrice, OrganizationResponse, SpendLimit,
    UpdateOrganizationLimitsResponse,
};
use api::routes::workspaces::WorkspaceResponse;
use chrono::{DateTime, Utc};
use database::repositories::credit_allocation::settle_unfunded_usage;
use database::repositories::CreditAllocationPolicy;
use database::Database;
use std::sync::Arc;
use tokio::sync::OnceCell;
use uuid::Uuid;

static SEED_DB: OnceCell<Arc<Database>> = OnceCell::const_new();

/// Shared database handle for seeding. Created lazily, once per process.
pub async fn seed_database() -> Arc<Database> {
    SEED_DB
        .get_or_init(|| async { Arc::new(Database::new(db_setup::create_test_pool().await)) })
        .await
        .clone()
}

/// The user a mock session token resolves to.
pub fn user_id_for_session(session_id: &str) -> Uuid {
    session_id
        .strip_prefix("rt_")
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .unwrap_or_else(|| Uuid::parse_str(MOCK_USER_ID).expect("fixed mock user ID must be valid"))
}

/// The email the mock auth reports for every session user.
const MOCK_ADMIN_EMAIL: &str = "admin@test.com";

/// Create an organization owned by the session's user, with the default workspace.
pub async fn seed_organization(session_id: &str) -> OrganizationResponse {
    let database = seed_database().await;
    let owner_id = user_id_for_session(session_id);
    let org_id = Uuid::new_v4();
    let name = Uuid::new_v4().to_string();
    let description = "A test organization".to_string();

    let mut client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let transaction = client.transaction().await.expect("begin transaction");

    let row = transaction
        .query_one(
            "INSERT INTO organizations (id, name, description, created_at, updated_at, is_active)
             VALUES ($1, $2, $3, NOW(), NOW(), true)
             RETURNING created_at, updated_at, settings, is_active",
            &[&org_id, &name, &Some(description.clone())],
        )
        .await
        .expect("Failed to seed organization");
    let created_at: DateTime<Utc> = row.get("created_at");
    let updated_at: DateTime<Utc> = row.get("updated_at");
    let settings: Option<serde_json::Value> = row.get("settings");
    let is_active: bool = row.get("is_active");

    transaction
        .execute(
            "INSERT INTO organization_members (organization_id, user_id, role, joined_at)
             VALUES ($1, $2, 'owner', NOW())",
            &[&org_id, &owner_id],
        )
        .await
        .expect("Failed to seed organization owner");

    transaction
        .execute(
            "INSERT INTO workspaces (
                 id, name, description, organization_id,
                 created_by_user_id, created_at, updated_at, is_active
             ) VALUES ($1, 'default', $2, $3, $4, NOW(), NOW(), true)",
            &[
                &Uuid::new_v4(),
                &Some(format!("Default workspace for {name}")),
                &org_id,
                &owner_id,
            ],
        )
        .await
        .expect("Failed to seed default workspace");

    transaction
        .commit()
        .await
        .expect("commit seeded organization");

    OrganizationResponse {
        id: org_id.to_string(),
        name,
        description: Some(description),
        owner_id: owner_id.to_string(),
        settings: settings.unwrap_or_default(),
        role: api::models::MemberRole::Owner,
        is_active,
        created_at,
        updated_at,
    }
}

/// Replace the active limit of one credit type, as the admin limits update did:
/// close the active row, insert the new one, then settle any unfunded usage.
#[allow(clippy::too_many_arguments)]
pub async fn seed_credit_limit(
    session_id: &str,
    org_id: &str,
    credit_type: &str,
    source: Option<&str>,
    amount_nano_dollars: i64,
    currency: &str,
    changed_by: &str,
    change_reason: &str,
) -> UpdateOrganizationLimitsResponse {
    let database = seed_database().await;
    let organization_id = Uuid::parse_str(org_id).expect("organization id must be a UUID");
    let changed_by_user_id = user_id_for_session(session_id);
    let currency = currency.to_uppercase();
    let source = source.map(str::to_string);

    let mut client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let transaction = client.transaction().await.expect("begin transaction");

    let org = transaction
        .query_opt(
            "SELECT 1 FROM organizations WHERE id = $1 AND is_active = true FOR UPDATE",
            &[&organization_id],
        )
        .await
        .expect("lock organization");
    assert!(org.is_some(), "Organization not found: {organization_id}");

    let now = Utc::now();
    transaction
        .execute(
            "UPDATE organization_limits_history
             SET effective_until = $1
             WHERE organization_id = $2 AND credit_type = $3 AND effective_until IS NULL",
            &[&now, &organization_id, &credit_type],
        )
        .await
        .expect("close previous limit");

    let row = transaction
        .query_one(
            "INSERT INTO organization_limits_history (
                 organization_id, spend_limit, credit_type, source, currency,
                 effective_from, changed_by, change_reason,
                 changed_by_user_id, changed_by_user_email
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             RETURNING effective_from",
            &[
                &organization_id,
                &amount_nano_dollars,
                &credit_type,
                &source,
                &currency,
                &now,
                &Some(changed_by.to_string()),
                &Some(change_reason.to_string()),
                &Some(changed_by_user_id),
                &Some(MOCK_ADMIN_EMAIL.to_string()),
            ],
        )
        .await
        .expect("Failed to seed credit limit");
    let effective_from: DateTime<Utc> = row.get("effective_from");

    settle_unfunded_usage(
        &transaction,
        organization_id,
        &CreditAllocationPolicy::default(),
    )
    .await
    .expect("settle unfunded usage");

    transaction.commit().await.expect("commit credit limit");

    UpdateOrganizationLimitsResponse {
        organization_id: organization_id.to_string(),
        credit_type: credit_type
            .parse()
            .expect("credit type must be a known credit type"),
        source,
        spend_limit: SpendLimit {
            amount: amount_nano_dollars,
            scale: 9,
            currency,
        },
        updated_at: effective_from.to_rfc3339(),
    }
}

/// Active workspaces of an organization, oldest first.
pub async fn seed_list_workspaces(org_id: &str) -> Vec<WorkspaceResponse> {
    let database = seed_database().await;
    let organization_id = Uuid::parse_str(org_id).expect("organization id must be a UUID");
    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let rows = client
        .query(
            "SELECT id, name, description, organization_id, created_by_user_id,
                    created_at, updated_at, is_active, settings
             FROM workspaces
             WHERE organization_id = $1 AND is_active = true
             ORDER BY created_at ASC",
            &[&organization_id],
        )
        .await
        .expect("Failed to list workspaces");
    rows.iter()
        .map(|row| WorkspaceResponse {
            id: row.get::<_, Uuid>("id").to_string(),
            name: row.get("name"),
            description: row.get("description"),
            organization_id: row.get::<_, Uuid>("organization_id").to_string(),
            created_by_user_id: row.get::<_, Uuid>("created_by_user_id").to_string(),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            is_active: row.get("is_active"),
            settings: row.get("settings"),
        })
        .collect()
}

/// Create an API key in a workspace, hashed and prefixed by `ApiKeyRepository`.
pub async fn seed_api_key(workspace_id: &str, name: String, session_id: &str) -> ApiKeyResponse {
    let database = seed_database().await;
    let created_by = user_id_for_session(session_id);
    let (key, api_key) = database
        .api_keys
        .create(services::workspace::CreateApiKeyRequest {
            name,
            workspace_id: services::workspace::WorkspaceId(
                Uuid::parse_str(workspace_id).expect("workspace id must be a UUID"),
            ),
            created_by_user_id: services::auth::ports::UserId(created_by),
            expires_at: Some(Utc::now() + chrono::Duration::days(90)),
            spend_limit: None,
        })
        .await
        .expect("Failed to seed API key");

    let formatted_prefix = if api_key.key_prefix.len() > 6 {
        format!("{}****", &api_key.key_prefix[..6])
    } else {
        format!("{}****", api_key.key_prefix)
    };
    let price = |amount| DecimalPrice {
        amount,
        scale: 9,
        currency: "USD".to_string(),
    };
    ApiKeyResponse {
        id: api_key.id.to_string(),
        name: Some(api_key.name.clone()),
        key: Some(key),
        key_prefix: formatted_prefix,
        workspace_id: api_key.workspace_id.to_string(),
        created_by_user_id: api_key.created_by_user_id.to_string(),
        created_at: api_key.created_at,
        last_used_at: api_key.last_used_at,
        expires_at: api_key.expires_at,
        spend_limit: api_key.spend_limit.map(price),
        is_active: api_key.is_active,
        deleted_at: api_key.deleted_at,
        usage: Some(price(api_key.usage)),
    }
}
