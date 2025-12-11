use std::env;
use tokio_postgres::NoTls;
use tracing::{error, info, warn};

/// Get test database name from environment or default
pub fn get_test_db_name() -> String {
    env::var("TEST_DATABASE_NAME").unwrap_or_else(|_| "platform_api_test".to_string())
}

/// Get admin database name - try 'postgres' first, fallback to 'template1' if not available
async fn get_admin_db_name(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<String, String> {
    // Try 'postgres' first (most common)
    if can_connect_to_db(host, port, username, password, "postgres").await {
        return Ok("postgres".to_string());
    }

    // Fallback to 'template1' (always exists in PostgreSQL)
    if can_connect_to_db(host, port, username, password, "template1").await {
        warn!("'postgres' database not found, using 'template1' as admin database");
        return Ok("template1".to_string());
    }

    Err("Neither 'postgres' nor 'template1' database found".to_string())
}

async fn can_connect_to_db(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    dbname: &str,
) -> bool {
    let conn_string =
        format!("host={host} port={port} user={username} password={password} dbname={dbname}");
    tokio_postgres::connect(&conn_string, NoTls).await.is_ok()
}

pub async fn reset_test_database(config: &config::DatabaseConfig) -> Result<(), String> {
    let test_db_name = get_test_db_name();

    // Safety check - only allow resetting test database
    if !test_db_name.contains("test") {
        panic!("Safety: Can only reset databases with 'test' in the name. Got: {test_db_name}");
    }

    let host = config
        .host
        .clone()
        .unwrap_or_else(|| "localhost".to_string());
    let port = config.port;
    let username = config.username.clone();
    let password = config.password.clone();

    // Find available admin database
    let admin_db = get_admin_db_name(&host, port, &username, &password).await?;

    let conn_string =
        format!("host={host} port={port} user={username} password={password} dbname={admin_db}");

    let (client, connection) = tokio_postgres::connect(&conn_string, NoTls)
        .await
        .map_err(|e| format!("Failed to connect to admin database: {e}"))?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            error!("Database connection error: {}", e);
        }
    });

    // Terminate existing connections to allow DROP
    let _ = client
        .execute(
            &format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
             WHERE datname = '{test_db_name}' AND pid <> pg_backend_pid()"
            ),
            &[],
        )
        .await;

    // Drop database if exists
    let drop_result = client
        .execute(&format!("DROP DATABASE IF EXISTS {test_db_name}"), &[])
        .await;

    if let Err(e) = drop_result {
        warn!("Failed to drop test database (may not exist): {}", e);
    }

    // Create fresh database
    client
        .execute(&format!("CREATE DATABASE {test_db_name}"), &[])
        .await
        .map_err(|e| format!("Failed to create test database: {e}"))?;

    info!("Test database '{}' reset successfully", test_db_name);
    Ok(())
}
