use database::DbPool;
use deadpool::Runtime;
use deadpool_postgres::Config;
use tokio_postgres::NoTls;

#[path = "schema.rs"]
mod schema;

pub async fn test_pool() -> anyhow::Result<DbPool> {
    schema::ensure_schema().await?;
    Ok(DbPool::new(
        pool_config().create_pool(Some(Runtime::Tokio1), NoTls)?,
    ))
}

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
