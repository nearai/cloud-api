#[allow(dead_code)]
mod support;

use database::repositories::{
    ApiKeyRepository, OrganizationUsageRepository, PgAnalyticsRepository,
};
use database::{ensure_spend_counters_ready, DbPool};
use deadpool::Runtime;
use deadpool_postgres::{Config, PoolConfig};
use services::admin::AnalyticsRepository;
use services::workspace::ports::{ApiKeyOrderBy, ApiKeyOrderDirection};
use support::pool_config;
use tokio_postgres::NoTls;
use uuid::Uuid;

async fn new_pool(config: &Config) -> anyhow::Result<DbPool> {
    let mut config = config.clone();
    config.pool = Some(PoolConfig::new(4));
    Ok(DbPool::new(
        config.create_pool(Some(Runtime::Tokio1), NoTls)?,
    ))
}

async fn scoped_pool() -> anyhow::Result<(DbPool, DbPool, String)> {
    let admin_pool = new_pool(&pool_config()).await?;
    let admin = admin_pool.get().await?;
    let schema = format!("counter_readers_{}", Uuid::new_v4().simple());
    admin
        .batch_execute(
            format!(
                r#"
                CREATE SCHEMA {schema};
                CREATE TABLE {schema}.api_keys (
                    id UUID PRIMARY KEY,
                    key_hash TEXT NOT NULL,
                    key_prefix TEXT NOT NULL,
                    name TEXT NOT NULL,
                    workspace_id UUID NOT NULL,
                    created_by_user_id UUID NOT NULL,
                    created_at TIMESTAMPTZ NOT NULL,
                    expires_at TIMESTAMPTZ,
                    last_used_at TIMESTAMPTZ,
                    is_active BOOLEAN NOT NULL,
                    deleted_at TIMESTAMPTZ,
                    spend_limit BIGINT
                );
                CREATE TABLE {schema}.workspaces (
                    id UUID PRIMARY KEY,
                    organization_id UUID NOT NULL
                );
                CREATE TABLE {schema}.api_key_spend (
                    api_key_id UUID PRIMARY KEY,
                    inference_spent BIGINT NOT NULL,
                    service_spent BIGINT NOT NULL,
                    updated_at TIMESTAMPTZ NOT NULL
                );
                CREATE TABLE {schema}.organizations (
                    id UUID PRIMARY KEY,
                    is_active BOOLEAN NOT NULL
                );
                CREATE TABLE {schema}.organization_balance (
                    organization_id UUID PRIMARY KEY,
                    total_spent BIGINT NOT NULL,
                    inference_spent BIGINT NOT NULL,
                    service_spent BIGINT NOT NULL,
                    spend_counters_ready_at TIMESTAMPTZ DEFAULT NOW()
                );
                CREATE TABLE {schema}.organization_limits_history (
                    organization_id UUID NOT NULL,
                    spend_limit BIGINT NOT NULL,
                    credit_type TEXT NOT NULL,
                    effective_until TIMESTAMPTZ,
                    source TEXT
                );
                "#
            )
            .as_str(),
        )
        .await?;
    drop(admin);

    let mut scoped_config = pool_config();
    scoped_config.options = Some(format!("-c search_path={schema},pg_catalog"));
    let scoped_pool = new_pool(&scoped_config).await?;
    Ok((admin_pool, scoped_pool, schema))
}

async fn drop_schema(
    admin_pool: DbPool,
    scoped_pool: DbPool,
    schema: String,
) -> anyhow::Result<()> {
    drop(scoped_pool);
    let admin = admin_pool.get().await?;
    admin
        .batch_execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await?;
    Ok(())
}

// A spawned task lets cleanup run after either a returned error or an assertion panic.
async fn with_scoped_pool<F, Fut>(test: F) -> anyhow::Result<()>
where
    F: FnOnce(DbPool) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let (admin_pool, pool, schema) = scoped_pool().await?;
    let result = tokio::spawn(test(pool.clone())).await;
    let cleanup = drop_schema(admin_pool, pool, schema).await;
    result??;
    cleanup
}

/// A workspace in a ready organization: readers must take the counter path.
async fn insert_ready_workspace(pool: &DbPool, workspace_id: Uuid) -> anyhow::Result<()> {
    let organization_id = Uuid::new_v4();
    let client = pool.get().await?;
    client
        .execute(
            "INSERT INTO organizations (id, is_active) VALUES ($1, true)",
            &[&organization_id],
        )
        .await?;
    client
        .execute(
            "INSERT INTO organization_balance (organization_id, total_spent, inference_spent, service_spent)
             VALUES ($1, 0, 0, 0)",
            &[&organization_id],
        )
        .await?;
    client
        .execute(
            "INSERT INTO workspaces (id, organization_id) VALUES ($1, $2)",
            &[&workspace_id, &organization_id],
        )
        .await?;
    Ok(())
}

async fn insert_key(
    pool: &DbPool,
    workspace_id: Uuid,
    key_id: Uuid,
    name: &str,
    inference_spent: i64,
    service_spent: i64,
    deleted: bool,
) -> anyhow::Result<()> {
    let client = pool.get().await?;
    let now = chrono::Utc::now();
    let deleted_at = deleted.then_some(now);
    client
        .execute(
            "INSERT INTO api_keys (id, key_hash, key_prefix, name, workspace_id, created_by_user_id, created_at, is_active, deleted_at) VALUES ($1, $2, $3, $4, $5, $6, $7, true, $8)",
            &[
                &key_id,
                &format!("hash-{key_id}"),
                &"sk-test",
                &name,
                &workspace_id,
                &Uuid::new_v4(),
                &now,
                &deleted_at,
            ],
        )
        .await?;
    client
        .execute(
            "INSERT INTO api_key_spend (api_key_id, inference_spent, service_spent, updated_at) VALUES ($1, $2, $3, $4)",
            &[&key_id, &inference_spent, &service_spent, &now],
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn key_list_reads_counter_cohort_with_stable_pagination() -> anyhow::Result<()> {
    with_scoped_pool(|pool| async move {
        let workspace_id = Uuid::new_v4();
        let zero = Uuid::new_v4();
        let inference = Uuid::new_v4();
        let service = Uuid::new_v4();
        let mixed = Uuid::new_v4();
        let deleted = Uuid::new_v4();
        let other_workspace = Uuid::new_v4();
        let other_workspace_key = Uuid::new_v4();
        insert_ready_workspace(&pool, workspace_id).await?;
        insert_ready_workspace(&pool, other_workspace).await?;
        insert_key(&pool, workspace_id, zero, "zero", 0, 0, false).await?;
        insert_key(&pool, workspace_id, inference, "inference", 30, 0, false).await?;
        insert_key(&pool, workspace_id, service, "service", 0, 40, false).await?;
        insert_key(&pool, workspace_id, mixed, "mixed", 50, 70, false).await?;
        insert_key(&pool, workspace_id, deleted, "deleted", 900, 900, true).await?;
        insert_key(
            &pool,
            other_workspace,
            other_workspace_key,
            "other-workspace",
            9_000,
            9_000,
            false,
        )
        .await?;
        pool.get()
            .await?
            .execute("DELETE FROM api_key_spend WHERE api_key_id = $1", &[&zero])
            .await?;

        // The isolated schema deliberately has no raw usage tables.
        ensure_spend_counters_ready(&pool).await?;
        let repository = ApiKeyRepository::new(pool.clone());
        let first_page = repository
            .list_by_workspace_paginated(
                workspace_id,
                3,
                0,
                Some(ApiKeyOrderBy::Usage),
                Some(ApiKeyOrderDirection::Desc),
            )
            .await?;
        assert_eq!(
            first_page.iter().map(|key| key.id).collect::<Vec<_>>(),
            vec![mixed, service, inference]
        );
        assert_eq!(
            first_page.iter().map(|key| key.usage).collect::<Vec<_>>(),
            vec![120, 40, 30]
        );

        let second_page = repository
            .list_by_workspace_paginated(
                workspace_id,
                3,
                3,
                Some(ApiKeyOrderBy::Usage),
                Some(ApiKeyOrderDirection::Desc),
            )
            .await?;
        assert_eq!(
            second_page.iter().map(|key| key.id).collect::<Vec<_>>(),
            vec![zero]
        );
        assert_eq!(second_page[0].usage, 0);
        assert!(!second_page.iter().any(|key| key.id == deleted));

        let ascending = repository
            .list_by_workspace_paginated(
                workspace_id,
                2,
                0,
                Some(ApiKeyOrderBy::Usage),
                Some(ApiKeyOrderDirection::Asc),
            )
            .await?;
        assert_eq!(
            ascending.iter().map(|key| key.id).collect::<Vec<_>>(),
            vec![zero, inference]
        );
        let past_end = repository
            .list_by_workspace_paginated(
                workspace_id,
                2,
                99,
                Some(ApiKeyOrderBy::Usage),
                Some(ApiKeyOrderDirection::Desc),
            )
            .await?;
        assert!(past_end.is_empty());
        assert!(!first_page.iter().any(|key| key.id == other_workspace_key));

        Ok(())
    })
    .await
}

#[tokio::test]
async fn admission_spend_reads_inference_counter_and_excludes_service_counter() -> anyhow::Result<()>
{
    with_scoped_pool(|pool| async move {
        let inference_key = Uuid::new_v4();
        let service_key = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        insert_ready_workspace(&pool, workspace_id).await?;
        insert_key(
            &pool,
            workspace_id,
            inference_key,
            "inference",
            321,
            0,
            false,
        )
        .await?;
        insert_key(&pool, workspace_id, service_key, "service", 0, 654, false).await?;

        ensure_spend_counters_ready(&pool).await?;
        let repository = OrganizationUsageRepository::new(pool.clone());
        assert_eq!(repository.get_api_key_spend(inference_key).await?, 321);
        assert_eq!(repository.get_api_key_spend(service_key).await?, 0);
        assert_eq!(repository.get_api_key_spend(Uuid::new_v4()).await?, 0);

        Ok(())
    })
    .await
}

#[tokio::test]
async fn billing_summary_reads_balance_splits_and_preserves_legacy_total() -> anyhow::Result<()> {
    with_scoped_pool(|pool| async move {
    let organization_id = Uuid::new_v4();
    let client = pool.get().await?;
    client
        .execute(
            "INSERT INTO organizations (id, is_active) VALUES ($1, true)",
            &[&organization_id],
        )
        .await?;
    client
        .execute(
            "INSERT INTO organization_limits_history (organization_id, spend_limit, credit_type, source) VALUES ($1, 0, 'payment', 'test')",
            &[&organization_id],
        )
        .await?;
    client
        .execute(
            "INSERT INTO organization_balance (organization_id, total_spent, inference_spent, service_spent) VALUES ($1, 123, 35, 20)",
            &[&organization_id],
        )
        .await?;
    drop(client);

    ensure_spend_counters_ready(&pool).await?;
    let summary = PgAnalyticsRepository::new(pool.clone())
        .get_billing_summary()
        .await?;
    assert_eq!(summary.total_consumed_usd, 123e-9);
    assert_eq!(summary.inference_consumed_usd, 35e-9);
    assert_eq!(summary.service_consumed_usd, 20e-9);

    Ok(())
    }).await
}
