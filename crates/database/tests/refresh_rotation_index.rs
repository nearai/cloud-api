use database::{ensure_refresh_rotation_index, DbPool};
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

#[tokio::test]
async fn refresh_rotation_startup_requires_ready_index_on_refresh_tokens() -> anyhow::Result<()> {
    let admin_pool: DbPool = pool_config()
        .create_pool(Some(Runtime::Tokio1), NoTls)?
        .into();
    let admin = admin_pool.get().await?;
    let schema = format!("rotation_indexes_{}", Uuid::new_v4().simple());
    admin
        .batch_execute(
            format!(
                "CREATE SCHEMA {schema}; \
                 CREATE TABLE {schema}.refresh_tokens (previous_token_hash VARCHAR(64));"
            )
            .as_str(),
        )
        .await?;

    let mut scoped_config = pool_config();
    scoped_config.options = Some(format!("-c search_path={schema}"));
    let scoped_pool: DbPool = scoped_config
        .create_pool(Some(Runtime::Tokio1), NoTls)?
        .into();
    let missing = ensure_refresh_rotation_index(&scoped_pool)
        .await
        .expect_err("missing index must fail the startup gate");
    assert!(missing
        .to_string()
        .contains("idx_refresh_tokens_previous_hash"));

    admin
        .batch_execute(
            format!(
                "CREATE INDEX idx_refresh_tokens_previous_hash \
                 ON {schema}.refresh_tokens (previous_token_hash) \
                 WHERE previous_token_hash IS NOT NULL;"
            )
            .as_str(),
        )
        .await?;
    ensure_refresh_rotation_index(&scoped_pool).await?;

    drop(scoped_pool);
    admin
        .batch_execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await?;
    Ok(())
}
