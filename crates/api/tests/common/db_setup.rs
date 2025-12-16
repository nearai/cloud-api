use database::Database;
use std::env;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use tracing::{debug, error, info, warn};

/// Template database name - used as source for creating per-test databases
const TEMPLATE_DB_NAME: &str = "platform_api_test_template";

/// Global once cell to ensure template database is created only once
static TEMPLATE_INITIALIZED: OnceCell<()> = OnceCell::const_new();

/// Get test database name from environment or default (legacy function for backwards compatibility)
pub fn get_test_db_name() -> String {
    env::var("TEST_DATABASE_NAME").unwrap_or_else(|_| "platform_api_test".to_string())
}

/// Get the template database name
pub fn get_template_db_name() -> String {
    env::var("TEST_TEMPLATE_DATABASE_NAME").unwrap_or_else(|_| TEMPLATE_DB_NAME.to_string())
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

/// Connect to the admin database and return the client
async fn connect_to_admin_db(
    config: &config::DatabaseConfig,
) -> Result<tokio_postgres::Client, String> {
    let host = config
        .host
        .clone()
        .unwrap_or_else(|| "localhost".to_string());
    let port = config.port;
    let username = config.username.clone();
    let password = config.password.clone();

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

    Ok(client)
}

/// Ensure the template database exists and has migrations applied.
/// This is called once per test run and cached via OnceCell.
pub async fn ensure_template_database(config: &config::DatabaseConfig) -> Result<(), String> {
    TEMPLATE_INITIALIZED
        .get_or_init(|| async {
            create_template_database_internal(config)
                .await
                .expect("Failed to create template database");
        })
        .await;
    Ok(())
}

/// Internal function to create the template database with migrations
async fn create_template_database_internal(config: &config::DatabaseConfig) -> Result<(), String> {
    let template_db_name = get_template_db_name();

    // Safety check
    if !template_db_name.contains("test") {
        panic!("Safety: Template database name must contain 'test'. Got: {template_db_name}");
    }

    info!("Creating template database '{}'...", template_db_name);

    let client = connect_to_admin_db(config).await?;

    // Terminate existing connections to the template database
    let _ = client
        .execute(
            &format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                 WHERE datname = '{template_db_name}' AND pid <> pg_backend_pid()"
            ),
            &[],
        )
        .await;

    // Drop template database if exists (to ensure fresh state)
    let _ = client
        .execute(&format!("DROP DATABASE IF EXISTS {template_db_name}"), &[])
        .await;

    // Create fresh template database
    client
        .execute(&format!("CREATE DATABASE {template_db_name}"), &[])
        .await
        .map_err(|e| format!("Failed to create template database: {e}"))?;

    info!(
        "Template database '{}' created, running migrations...",
        template_db_name
    );

    // Connect to template database and run migrations
    let template_config = config::DatabaseConfig {
        database: template_db_name.clone(),
        ..config.clone()
    };

    let database = Arc::new(
        Database::from_config(&template_config)
            .await
            .map_err(|e| format!("Failed to connect to template database: {e}"))?,
    );

    database
        .run_migrations()
        .await
        .map_err(|e| format!("Failed to run migrations on template database: {e}"))?;

    info!(
        "Template database '{}' initialized with migrations",
        template_db_name
    );

    Ok(())
}

/// Create a unique test database from the template.
/// Returns the name of the created database.
pub async fn create_test_database_from_template(
    config: &config::DatabaseConfig,
    test_id: &str,
) -> Result<String, String> {
    // Ensure template exists first
    ensure_template_database(config).await?;

    let template_db_name = get_template_db_name();
    // Sanitize test_id to be a valid database name (replace hyphens with underscores)
    let sanitized_id = test_id.replace('-', "_");
    let test_db_name = format!("test_{sanitized_id}");

    // Safety check
    if !test_db_name.starts_with("test_") {
        panic!("Safety: Test database name must start with 'test_'. Got: {test_db_name}");
    }

    debug!("Creating test database '{}' from template...", test_db_name);

    let client = connect_to_admin_db(config).await?;

    // Terminate any existing connections to this test database (in case of leftover from crashed test)
    let _ = client
        .execute(
            &format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                 WHERE datname = '{test_db_name}' AND pid <> pg_backend_pid()"
            ),
            &[],
        )
        .await;

    // Drop if exists (cleanup from previous failed test)
    let _ = client
        .execute(&format!("DROP DATABASE IF EXISTS {test_db_name}"), &[])
        .await;

    // Create database from template (this is very fast in PostgreSQL)
    client
        .execute(
            &format!("CREATE DATABASE {test_db_name} TEMPLATE {template_db_name}"),
            &[],
        )
        .await
        .map_err(|e| format!("Failed to create test database from template: {e}"))?;

    debug!("Test database '{}' created from template", test_db_name);

    Ok(test_db_name)
}

/// Drop a test database after test completion.
/// This should be called during test cleanup.
pub async fn drop_test_database(
    config: &config::DatabaseConfig,
    db_name: &str,
) -> Result<(), String> {
    // Safety check - only allow dropping test databases
    if !db_name.starts_with("test_") {
        panic!("Safety: Can only drop databases starting with 'test_'. Got: {db_name}");
    }

    debug!("Dropping test database '{}'...", db_name);

    let client = connect_to_admin_db(config).await?;

    // Terminate existing connections
    let _ = client
        .execute(
            &format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                 WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
            ),
            &[],
        )
        .await;

    // Drop the database
    client
        .execute(&format!("DROP DATABASE IF EXISTS {db_name}"), &[])
        .await
        .map_err(|e| format!("Failed to drop test database: {e}"))?;

    debug!("Test database '{}' dropped", db_name);

    Ok(())
}
