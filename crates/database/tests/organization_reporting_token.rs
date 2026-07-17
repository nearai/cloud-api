use chrono::{Duration, Utc};
use database::{migrations, DbPool};
use deadpool::Runtime;
use deadpool_postgres::Config;
use services::common::REPORTING_TOKEN_PREFIX;
use services::reporting_tokens::ports::{
    CreateOrganizationReportingTokenRequest, OrganizationReportingTokenRepository as _,
    REPORTING_TOKEN_SCOPE_USAGE_READ,
};
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use uuid::Uuid;

static MIGRATED: OnceCell<()> = OnceCell::const_new();

fn pool_config() -> Config {
    let mut config = Config::new();
    config.host = Some(std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_string()));
    config.port = Some(
        std::env::var("PGPORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(5432),
    );
    config.dbname =
        Some(std::env::var("PGDATABASE").unwrap_or_else(|_| "platform_api".to_string()));
    config.user = Some(std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_string()));
    config.password = Some(std::env::var("PGPASSWORD").unwrap_or_else(|_| "postgres".to_string()));
    config
}

async fn test_pool() -> anyhow::Result<DbPool> {
    let pool = DbPool::new(pool_config().create_pool(Some(Runtime::Tokio1), NoTls)?);
    MIGRATED
        .get_or_try_init(|| async { migrations::run(&pool).await })
        .await?;
    Ok(pool)
}

async fn insert_org_and_user(pool: &DbPool) -> anyhow::Result<(Uuid, Uuid)> {
    let client = pool.get().await?;
    let org_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let suffix = Uuid::new_v4().simple().to_string();
    let now = Utc::now();

    client
        .execute(
            r#"
            INSERT INTO users (
                id, email, username, display_name, avatar_url, created_at, updated_at,
                last_login_at, is_active, auth_provider, provider_user_id
            )
            VALUES ($1, $2, $3, NULL, NULL, $4, $4, NULL, true, 'test', $5)
            "#,
            &[
                &user_id,
                &format!("reporting-{suffix}@example.test"),
                &format!("reporting-{suffix}"),
                &now,
                &format!("provider-{suffix}"),
            ],
        )
        .await?;

    client
        .execute(
            r#"
            INSERT INTO organizations (id, name, description, created_at, updated_at, is_active)
            VALUES ($1, $2, NULL, $3, $3, true)
            "#,
            &[&org_id, &format!("reporting-org-{suffix}"), &now],
        )
        .await?;

    Ok((org_id, user_id))
}

#[tokio::test]
async fn organization_reporting_token_create_validate_active() -> anyhow::Result<()> {
    // Given: an organization and creator exist.
    let pool = test_pool().await?;
    let repository =
        database::repositories::OrganizationReportingTokenRepository::new(pool.clone());
    let (organization_id, user_id) = insert_org_and_user(&pool).await?;
    let expires_at = Utc::now() + Duration::days(1);

    // When: a reporting token is created and then validated by its raw secret.
    let created = repository
        .create(CreateOrganizationReportingTokenRequest {
            organization_id,
            name: "warehouse sync".to_string(),
            created_by_user_id: user_id,
            expires_at: Some(expires_at),
        })
        .await?;
    let validated = repository.validate(&created.raw_token).await?;

    // Then: the raw token is returned once, only its hash/prefix is persisted, and validation
    // returns org-scoped usage-read context without exposing a raw token or hash.
    assert!(created.raw_token.starts_with(REPORTING_TOKEN_PREFIX));
    assert!(created
        .token
        .token_prefix
        .starts_with(REPORTING_TOKEN_PREFIX));
    assert_ne!(created.token.token_prefix, created.raw_token);
    assert_eq!(created.token.scope, REPORTING_TOKEN_SCOPE_USAGE_READ);

    let row = pool
        .get()
        .await?
        .query_one(
            r#"
            SELECT token_hash, token_prefix
            FROM organization_reporting_tokens
            WHERE id = $1
            "#,
            &[&created.token.id],
        )
        .await?;
    let token_hash: String = row.get("token_hash");
    let token_prefix: String = row.get("token_prefix");
    assert_ne!(token_hash, created.raw_token);
    assert_eq!(token_prefix, created.token.token_prefix);

    let validated = validated.expect("active reporting token should validate");
    assert_eq!(validated.id, created.token.id);
    assert_eq!(validated.organization_id, organization_id);
    assert_eq!(validated.token_prefix, created.token.token_prefix);
    assert_eq!(validated.scope, REPORTING_TOKEN_SCOPE_USAGE_READ);
    assert!(validated.last_used_at.is_some());

    Ok(())
}

#[tokio::test]
async fn organization_reporting_token_rejects_revoked_and_expired() -> anyhow::Result<()> {
    // Given: one expired reporting token and one active reporting token.
    let pool = test_pool().await?;
    let repository =
        database::repositories::OrganizationReportingTokenRepository::new(pool.clone());
    let (organization_id, user_id) = insert_org_and_user(&pool).await?;

    let expired = repository
        .create(CreateOrganizationReportingTokenRequest {
            organization_id,
            name: "expired sync".to_string(),
            created_by_user_id: user_id,
            expires_at: Some(Utc::now() - Duration::minutes(1)),
        })
        .await?;
    let revoked = repository
        .create(CreateOrganizationReportingTokenRequest {
            organization_id,
            name: "revoked sync".to_string(),
            created_by_user_id: user_id,
            expires_at: Some(Utc::now() + Duration::days(1)),
        })
        .await?;

    // When: the active token is revoked and both secrets are validated.
    assert!(repository.revoke(revoked.token.id, user_id).await?);

    // Then: expired and revoked reporting tokens are rejected.
    assert!(repository.validate(&expired.raw_token).await?.is_none());
    assert!(repository.validate(&revoked.raw_token).await?.is_none());

    Ok(())
}

#[tokio::test]
async fn organization_reporting_token_rejects_malformed_inputs_and_empty_name() -> anyhow::Result<()>
{
    // Given: an organization and creator exist.
    let pool = test_pool().await?;
    let repository =
        database::repositories::OrganizationReportingTokenRepository::new(pool.clone());
    let (organization_id, user_id) = insert_org_and_user(&pool).await?;

    // When: malformed token material and an empty token name are submitted.
    let malformed = repository.validate("sk-not-a-reporting-token").await?;
    let empty_name = repository
        .create(CreateOrganizationReportingTokenRequest {
            organization_id,
            name: "   ".to_string(),
            created_by_user_id: user_id,
            expires_at: None,
        })
        .await;

    // Then: malformed tokens do not validate and empty names fail validation.
    assert!(malformed.is_none());
    assert!(empty_name.is_err());

    Ok(())
}

#[tokio::test]
async fn organization_reporting_token_validation_preserves_recent_last_used_at(
) -> anyhow::Result<()> {
    // Given: an active token whose audit timestamp was refreshed less than 15 minutes ago.
    let pool = test_pool().await?;
    let repository =
        database::repositories::OrganizationReportingTokenRepository::new(pool.clone());
    let (organization_id, user_id) = insert_org_and_user(&pool).await?;
    let created = repository
        .create(CreateOrganizationReportingTokenRequest {
            organization_id,
            name: "recent sync".to_string(),
            created_by_user_id: user_id,
            expires_at: None,
        })
        .await?;
    let client = pool.get().await?;
    client
        .execute(
            r#"
            UPDATE organization_reporting_tokens
            SET last_used_at = NOW() - INTERVAL '5 minutes'
            WHERE id = $1
            "#,
            &[&created.token.id],
        )
        .await?;
    let row_before = client
        .query_one(
            r#"
            SELECT last_used_at, xmin::text::bigint AS row_version
            FROM organization_reporting_tokens
            WHERE id = $1
            "#,
            &[&created.token.id],
        )
        .await?;
    let persisted_before: chrono::DateTime<Utc> = row_before.get("last_used_at");
    let row_version_before: i64 = row_before.get("row_version");

    // When: the token is validated again inside the debounce interval.
    let validated = repository
        .validate(&created.raw_token)
        .await?
        .expect("active token should validate");

    // Then: validation returns the token without issuing another audit-timestamp write.
    let row_after = client
        .query_one(
            r#"
            SELECT last_used_at, xmin::text::bigint AS row_version
            FROM organization_reporting_tokens
            WHERE id = $1
            "#,
            &[&created.token.id],
        )
        .await?;
    let persisted_after: chrono::DateTime<Utc> = row_after.get("last_used_at");
    let row_version_after: i64 = row_after.get("row_version");
    assert_eq!(validated.last_used_at, Some(persisted_before));
    assert_eq!(persisted_after, persisted_before);
    assert_eq!(row_version_after, row_version_before);

    Ok(())
}

#[tokio::test]
async fn organization_reporting_token_validation_refreshes_stale_last_used_at() -> anyhow::Result<()>
{
    // Given: an active token whose audit timestamp is older than 15 minutes.
    let pool = test_pool().await?;
    let repository =
        database::repositories::OrganizationReportingTokenRepository::new(pool.clone());
    let (organization_id, user_id) = insert_org_and_user(&pool).await?;
    let created = repository
        .create(CreateOrganizationReportingTokenRequest {
            organization_id,
            name: "stale sync".to_string(),
            created_by_user_id: user_id,
            expires_at: None,
        })
        .await?;
    let client = pool.get().await?;
    client
        .execute(
            r#"
            UPDATE organization_reporting_tokens
            SET last_used_at = NOW() - INTERVAL '16 minutes'
            WHERE id = $1
            "#,
            &[&created.token.id],
        )
        .await?;
    let persisted_before: chrono::DateTime<Utc> = client
        .query_one(
            "SELECT last_used_at FROM organization_reporting_tokens WHERE id = $1",
            &[&created.token.id],
        )
        .await?
        .get("last_used_at");

    // When: the stale token is validated.
    let validated = repository
        .validate(&created.raw_token)
        .await?
        .expect("active token should validate");

    // Then: its audit timestamp is refreshed and returned.
    let persisted_after: chrono::DateTime<Utc> = client
        .query_one(
            "SELECT last_used_at FROM organization_reporting_tokens WHERE id = $1",
            &[&created.token.id],
        )
        .await?
        .get("last_used_at");
    assert_eq!(validated.last_used_at, Some(persisted_after));
    assert!(persisted_after > persisted_before);

    Ok(())
}
