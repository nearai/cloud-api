use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod, Runtime};
use tokio_postgres::NoTls;
use std::env;
use tracing::info;

/// Database configuration
#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
    pub max_connections: usize,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            host: env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string()),
            port: env::var("DB_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: env::var("DB_NAME").unwrap_or_else(|_| "platform_api".to_string()),
            username: env::var("DB_USER").unwrap_or_else(|_| "postgres".to_string()),
            password: env::var("DB_PASSWORD").unwrap_or_else(|_| "postgres".to_string()),
            max_connections: env::var("DB_MAX_CONNECTIONS")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(20),
        }
    }
}

impl DatabaseConfig {
    /// Create a new database configuration
    pub fn new(
        host: String,
        port: u16,
        database: String,
        username: String,
        password: String,
        max_connections: usize,
    ) -> Self {
        Self {
            host,
            port,
            database,
            username,
            password,
            max_connections,
        }
    }

    /// Create from environment variables with custom prefix
    pub fn from_env_with_prefix(prefix: &str) -> Self {
        Self {
            host: env::var(format!("{}_HOST", prefix))
                .unwrap_or_else(|_| "localhost".to_string()),
            port: env::var(format!("{}_PORT", prefix))
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: env::var(format!("{}_DATABASE", prefix))
                .unwrap_or_else(|_| "platform_api".to_string()),
            username: env::var(format!("{}_USERNAME", prefix))
                .unwrap_or_else(|_| "postgres".to_string()),
            password: env::var(format!("{}_PASSWORD", prefix))
                .unwrap_or_else(|_| "postgres".to_string()),
            max_connections: env::var(format!("{}_MAX_CONNECTIONS", prefix))
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(20),
        }
    }
}

/// Create a connection pool from configuration
pub async fn create_pool(config: &DatabaseConfig) -> anyhow::Result<Pool> {
    let mut cfg = Config::new();
    cfg.host = Some(config.host.clone());
    cfg.port = Some(config.port);
    cfg.dbname = Some(config.database.clone());
    cfg.user = Some(config.username.clone());
    cfg.password = Some(config.password.clone());
    cfg.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });
    
    let pool = cfg.create_pool(Some(Runtime::Tokio1), NoTls)
        .map_err(|e| anyhow::anyhow!("Failed to create pool: {}", e))?;
    
    info!(
        "Database connection pool created: {}:{}/{}",
        config.host, config.port, config.database
    );
    
    // Test the connection
    let client = pool.get().await
        .map_err(|e| anyhow::anyhow!("Failed to get connection from pool: {}", e))?;
    
    client.simple_query("SELECT 1").await
        .map_err(|e| anyhow::anyhow!("Failed to test database connection: {}", e))?;
    info!("Database connection test successful");
    
    Ok(pool)
}

/// Connection pool type alias
pub type DbPool = Pool;
