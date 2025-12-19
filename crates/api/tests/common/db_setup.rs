use database::Database;
use std::env;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use tracing::{debug, error, info, warn};

const TEMPLATE_DB_NAME: &str = "platform_api_test_template";
static TEMPLATE_INITIALIZED: OnceCell<()> = OnceCell::const_new();

/// Validate that a database identifier only contains safe characters (alphanumeric + underscore).
/// PostgreSQL identifiers with only these characters don't need quoting and are safe from injection.
/// This prevents any potential SQL injection via format! strings.
fn validate_db_identifier(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Database name cannot be empty".to_string());
    }
    if name.len() > 63 {
        return Err(format!("Database name too long (max 63 chars): {}", name));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "Database name contains invalid characters (only alphanumeric and _ allowed): {}",
            name
        ));
    }
    Ok(())
}

pub fn get_test_db_name() -> String {
    let name = env::var("TEST_DATABASE_NAME").unwrap_or_else(|_| "platform_api_test".to_string());
    validate_db_identifier(&name).unwrap_or_else(|e| panic!("Invalid TEST_DATABASE_NAME: {}", e));
    name
}

pub fn get_template_db_name() -> String {
    let name =
        env::var("TEST_TEMPLATE_DATABASE_NAME").unwrap_or_else(|_| TEMPLATE_DB_NAME.to_string());
    validate_db_identifier(&name)
        .unwrap_or_else(|e| panic!("Invalid TEST_TEMPLATE_DATABASE_NAME: {}", e));
    name
}

/// Get admin database name - try 'postgres' first, fallback to 'template1' if not available
async fn get_admin_db_name(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<String, String> {
    if can_connect_to_db(host, port, username, password, "postgres").await {
        return Ok("postgres".to_string());
    }

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

/// Advisory lock ID for template database creation (arbitrary but unique)
const TEMPLATE_DB_LOCK_ID: i64 = 0x5445_5354_5450_4C00;

/// Internal function to create the template database with migrations.
/// Uses PostgreSQL advisory locks to coordinate across multiple test processes.
async fn create_template_database_internal(config: &config::DatabaseConfig) -> Result<(), String> {
    let template_db_name = get_template_db_name();

    if check_template_database_ready(config, &template_db_name).await {
        debug!(
            "Template database '{}' already exists (fast path), skipping creation",
            template_db_name
        );
        return Ok(());
    }

    let client = connect_to_admin_db(config).await?;

    debug!("Acquiring advisory lock for template database creation...");
    client
        .execute(
            &format!("SELECT pg_advisory_lock({TEMPLATE_DB_LOCK_ID})"),
            &[],
        )
        .await
        .map_err(|e| format!("Failed to acquire advisory lock: {e}"))?;

    if check_template_database_ready(config, &template_db_name).await {
        debug!(
            "Template database '{}' already exists (after lock), skipping creation",
            template_db_name
        );
        let _ = client
            .execute(
                &format!("SELECT pg_advisory_unlock({TEMPLATE_DB_LOCK_ID})"),
                &[],
            )
            .await;
        return Ok(());
    }

    info!("Creating template database '{}'...", template_db_name);

    let _ = client
        .execute(
            &format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                 WHERE datname = '{template_db_name}' AND pid <> pg_backend_pid()"
            ),
            &[],
        )
        .await;

    let _ = client
        .execute(&format!("DROP DATABASE IF EXISTS {template_db_name}"), &[])
        .await;

    client
        .execute(&format!("CREATE DATABASE {template_db_name}"), &[])
        .await
        .map_err(|e| {
            let _ = futures::executor::block_on(client.execute(
                &format!("SELECT pg_advisory_unlock({TEMPLATE_DB_LOCK_ID})"),
                &[],
            ));
            format!("Failed to create template database: {e}")
        })?;

    info!(
        "Template database '{}' created, running migrations...",
        template_db_name
    );

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

    drop(database);
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let _ = client
        .execute(
            &format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                 WHERE datname = '{template_db_name}' AND pid <> pg_backend_pid()"
            ),
            &[],
        )
        .await;

    debug!("Terminated all connections to template database");

    let _ = client
        .execute(
            &format!("SELECT pg_advisory_unlock({TEMPLATE_DB_LOCK_ID})"),
            &[],
        )
        .await;

    Ok(())
}

/// Check if the template database exists by querying the admin database.
async fn check_template_database_ready(config: &config::DatabaseConfig, db_name: &str) -> bool {
    let client = match connect_to_admin_db(config).await {
        Ok(c) => c,
        Err(_) => return false,
    };

    let exists = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
            &[&db_name],
        )
        .await
        .map(|row| row.get::<_, bool>(0))
        .unwrap_or(false);

    exists
}

/// Create a unique test database from the template.
pub async fn create_test_database_from_template(
    config: &config::DatabaseConfig,
    test_id: &str,
) -> Result<String, String> {
    ensure_template_database(config).await?;

    let template_db_name = get_template_db_name();
    let sanitized_id = test_id.replace('-', "_");
    let test_db_name = format!("test_{sanitized_id}");

    // Validate the generated database name for safety
    validate_db_identifier(&test_db_name)
        .map_err(|e| format!("Invalid test database name: {}", e))?;

    debug!("Creating test database '{}' from template...", test_db_name);

    let client = connect_to_admin_db(config).await?;

    let _ = client
        .execute(
            &format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                 WHERE datname = '{test_db_name}' AND pid <> pg_backend_pid()"
            ),
            &[],
        )
        .await;

    let _ = client
        .execute(&format!("DROP DATABASE IF EXISTS {test_db_name}"), &[])
        .await;

    let _ = client
        .execute(
            &format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                 WHERE datname = '{template_db_name}' AND pid <> pg_backend_pid()"
            ),
            &[],
        )
        .await;

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
pub async fn drop_test_database(
    config: &config::DatabaseConfig,
    db_name: &str,
) -> Result<(), String> {
    // Validate database name to prevent injection
    validate_db_identifier(db_name).map_err(|e| format!("Invalid database name: {}", e))?;

    // Ensure it's a test database (safety check)
    if !db_name.starts_with("test_") {
        return Err(format!(
            "Safety check: Can only drop databases starting with 'test_'. Got: {}",
            db_name
        ));
    }

    debug!("Dropping test database '{}'...", db_name);

    let client = connect_to_admin_db(config).await?;

    let _ = client
        .execute(
            &format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                 WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
            ),
            &[],
        )
        .await;

    client
        .execute(&format!("DROP DATABASE IF EXISTS {db_name}"), &[])
        .await
        .map_err(|e| format!("Failed to drop test database: {e}"))?;

    debug!("Test database '{}' dropped", db_name);

    Ok(())
}

/// Drop ALL test databases (databases starting with 'test_').
/// This is useful for cleaning up orphaned test databases.
/// Returns the number of databases dropped.
pub async fn drop_all_test_databases(config: &config::DatabaseConfig) -> Result<usize, String> {
    info!("Cleaning up all test databases...");

    let client = connect_to_admin_db(config).await?;

    // Find all databases starting with 'test_'
    let rows = client
        .query(
            "SELECT datname FROM pg_database WHERE datname LIKE 'test_%'",
            &[],
        )
        .await
        .map_err(|e| format!("Failed to list test databases: {e}"))?;

    let mut dropped_count = 0;
    for row in rows {
        let db_name: String = row.get(0);

        // Skip the template database
        if db_name == get_template_db_name() {
            debug!("Skipping template database '{}'", db_name);
            continue;
        }

        // Validate database name (defensive check - should be valid from pg_database)
        if let Err(e) = validate_db_identifier(&db_name) {
            warn!("Skipping database with invalid name: {} ({})", db_name, e);
            continue;
        }

        // Terminate connections
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
        match client
            .execute(&format!("DROP DATABASE IF EXISTS {db_name}"), &[])
            .await
        {
            Ok(_) => {
                debug!("Dropped test database '{}'", db_name);
                dropped_count += 1;
            }
            Err(e) => {
                warn!("Failed to drop test database '{}': {}", db_name, e);
            }
        }
    }

    info!("Cleaned up {} test database(s)", dropped_count);

    Ok(dropped_count)
}
