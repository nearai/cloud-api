use database::{ensure_usage_reporting_indexes, DbPool};
use deadpool::Runtime;
use deadpool_postgres::Config;
use tokio_postgres::NoTls;
use uuid::Uuid;

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

fn create_pool(config: &Config) -> anyhow::Result<DbPool> {
    Ok(config.create_pool(Some(Runtime::Tokio1), NoTls)?.into())
}

#[tokio::test]
async fn usage_reporting_startup_check_requires_valid_indexes_on_expected_tables(
) -> anyhow::Result<()> {
    let admin_pool = create_pool(&pool_config())?;
    let admin = admin_pool.get().await?;
    let schema = format!("reporting_indexes_{}", Uuid::new_v4().simple());
    admin
        .batch_execute(
            format!(
                r#"
                CREATE SCHEMA {schema};
                CREATE TABLE {schema}.organization_usage_log (
                    id UUID, organization_id UUID, workspace_id UUID,
                    api_key_id UUID, created_at TIMESTAMPTZ
                );
                CREATE TABLE {schema}.organization_service_usage_log (
                    id UUID, organization_id UUID, workspace_id UUID,
                    api_key_id UUID, created_at TIMESTAMPTZ
                );
                "#
            )
            .as_str(),
        )
        .await?;

    let mut scoped_config = pool_config();
    scoped_config.options = Some(format!("-c search_path={schema}"));
    let scoped_pool = create_pool(&scoped_config)?;
    let missing = ensure_usage_reporting_indexes(&scoped_pool)
        .await
        .expect_err("tables without reporting indexes must fail the startup gate");
    assert!(missing
        .to_string()
        .contains("idx_org_usage_reporting_org_created_id"));

    admin
        .batch_execute(
            format!(
                r#"
                CREATE INDEX idx_org_usage_reporting_org_created_id
                    ON {schema}.organization_usage_log (organization_id, created_at DESC, id DESC);
                CREATE INDEX idx_org_usage_reporting_org_workspace_created_id
                    ON {schema}.organization_usage_log (organization_id, workspace_id, created_at DESC, id DESC);
                CREATE INDEX idx_org_usage_reporting_org_api_key_created_id
                    ON {schema}.organization_usage_log (organization_id, api_key_id, created_at DESC, id DESC);
                CREATE INDEX idx_org_service_usage_reporting_org_created_id
                    ON {schema}.organization_service_usage_log (organization_id, created_at DESC, id DESC);
                CREATE INDEX idx_org_service_usage_reporting_org_workspace_created_id
                    ON {schema}.organization_service_usage_log (organization_id, workspace_id, created_at DESC, id DESC);
                CREATE INDEX idx_org_service_usage_reporting_org_api_key_created_id
                    ON {schema}.organization_service_usage_log (organization_id, api_key_id, created_at DESC, id DESC);
                "#
            )
            .as_str(),
        )
        .await?;

    ensure_usage_reporting_indexes(&scoped_pool).await?;
    drop(scoped_pool);
    admin
        .batch_execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await?;
    Ok(())
}
