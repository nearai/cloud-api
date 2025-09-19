use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod, Runtime};
use std::env;
use tokio_postgres::NoTls;
use tracing::info;

/// Create a connection pool from configuration
pub async fn create_pool(config: &config::DatabaseConfig) -> anyhow::Result<Pool> {
    let mut cfg = Config::new();
    cfg.host = Some(config.host.clone());
    cfg.port = Some(config.port);
    cfg.dbname = Some(config.database.clone());
    cfg.user = Some(config.username.clone());
    cfg.password = Some(config.password.clone());
    cfg.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });

    let pool = cfg
        .create_pool(Some(Runtime::Tokio1), NoTls)
        .map_err(|e| anyhow::anyhow!("Failed to create pool: {}", e))?;

    info!(
        "Database connection pool created: {}:{}/{}",
        config.host, config.port, config.database
    );

    // Test the connection
    let client = pool
        .get()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get connection from pool: {}", e))?;

    client
        .simple_query("SELECT 1")
        .await
        .map_err(|e| anyhow::anyhow!("Failed to test database connection: {}", e))?;
    info!("Database connection test successful");

    Ok(pool)
}

/// Connection pool type alias
pub type DbPool = Pool;
