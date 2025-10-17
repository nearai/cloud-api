use crate::Database;
use anyhow::Result;
use std::env;

/// Create a mock database for testing
pub async fn create_mock_database() -> Result<Database> {
    // Check if we should use a local postgres for testing
    if env::var("TEST_DATABASE_URL").is_ok() {
        // If TEST_DATABASE_URL is set, use a real postgres connection
        let database_url = env::var("TEST_DATABASE_URL").unwrap();
        let (_client, connection) =
            tokio_postgres::connect(&database_url, tokio_postgres::NoTls).await?;

        // Spawn connection handler
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("connection error: {}", e);
            }
        });

        // Create a simple pool from the config
        let config = database_url.parse::<tokio_postgres::Config>()?;
        let mgr_config = deadpool_postgres::ManagerConfig {
            recycling_method: deadpool_postgres::RecyclingMethod::Fast,
        };
        let mgr =
            deadpool_postgres::Manager::from_config(config, tokio_postgres::NoTls, mgr_config);
        let pool = deadpool_postgres::Pool::builder(mgr).max_size(1).build()?;

        return Ok(Database::new(pool));
    }

    // Otherwise, create a dummy pool that will fail if actually used
    // This is for tests that don't actually need database operations
    let mut pg_config = tokio_postgres::Config::new();
    pg_config
        .host("mock-host-that-doesnt-exist")
        .port(5432)
        .dbname("mock_db")
        .user("mock_user")
        .password("mock_pass")
        .connect_timeout(std::time::Duration::from_millis(1)); // Fail fast if connection is attempted

    let mgr_config = deadpool_postgres::ManagerConfig {
        recycling_method: deadpool_postgres::RecyclingMethod::Fast,
    };
    let mgr = deadpool_postgres::Manager::from_config(pg_config, tokio_postgres::NoTls, mgr_config);

    // Create a pool with max_size of 0 so it doesn't try to connect immediately
    let pool = deadpool_postgres::Pool::builder(mgr).max_size(1).build()?;

    Ok(Database::new(pool))
}
