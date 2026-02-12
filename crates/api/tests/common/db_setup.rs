use database::Database;
use std::env;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use tracing::{debug, info};

static SHARED_DB_READY: OnceCell<()> = OnceCell::const_new();

fn db_host() -> String {
    env::var("DATABASE_HOST").unwrap_or_else(|_| "localhost".to_string())
}

fn db_port() -> u16 {
    env::var("DATABASE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5432)
}

fn db_user() -> String {
    env::var("DATABASE_USERNAME").unwrap_or_else(|_| "postgres".to_string())
}

fn db_password() -> String {
    env::var("DATABASE_PASSWORD").unwrap_or_else(|_| "postgres".to_string())
}

pub fn get_test_db_name() -> String {
    env::var("TEST_DATABASE_NAME").unwrap_or_else(|_| "platform_api_e2e".to_string())
}

/// Bootstrap the shared database once: create it if missing, run migrations, drop the bootstrap pool.
async fn ensure_shared_db() {
    SHARED_DB_READY
        .get_or_init(|| async {
            let db_name = get_test_db_name();
            let host = db_host();
            let port = db_port();
            let user = db_user();
            let password = db_password();

            // Connect to admin database to create the shared DB if it doesn't exist
            let admin_conn_string =
                format!("host={host} port={port} user={user} password={password} dbname=postgres");
            let (client, connection) = tokio_postgres::connect(&admin_conn_string, NoTls)
                .await
                .expect("Failed to connect to admin database for bootstrap");

            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    eprintln!("Admin connection error during bootstrap: {e}");
                }
            });

            let exists: bool = client
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
                    &[&db_name],
                )
                .await
                .expect("Failed to check if shared database exists")
                .get(0);

            if !exists {
                info!("Creating shared e2e database '{db_name}'...");
                // db_name is a simple env var default; no user-controlled injection risk in test code
                client
                    .execute(&format!("CREATE DATABASE {db_name}"), &[])
                    .await
                    .expect("Failed to create shared e2e database");
            }

            drop(client);

            // Run migrations via a temporary 2-connection pool
            let db_config = config::DatabaseConfig {
                primary_app_id: "postgres-test".to_string(),
                gateway_subdomain: "cvm1.near.ai".to_string(),
                port,
                host: Some(host),
                database: db_name.clone(),
                username: user,
                password,
                max_connections: 2,
                tls_enabled: false,
                tls_ca_cert_path: None,
                refresh_interval: 30,
                mock: false,
            };

            let database = Arc::new(
                Database::from_config(&db_config)
                    .await
                    .expect("Failed to connect to shared e2e database for migrations"),
            );

            database
                .run_migrations()
                .await
                .expect("Failed to run migrations on shared e2e database");

            debug!("Shared e2e database '{db_name}' ready with migrations");
            drop(database);
        })
        .await;
}

/// Create a 4-connection deadpool pool to the shared e2e database.
/// Called once per test.
pub async fn create_test_pool() -> database::pool::DbPool {
    ensure_shared_db().await;

    let mut pg_config = deadpool_postgres::Config::new();
    pg_config.host = Some(db_host());
    pg_config.port = Some(db_port());
    pg_config.dbname = Some(get_test_db_name());
    pg_config.user = Some(db_user());
    pg_config.password = Some(db_password());

    pg_config.pool = Some(deadpool_postgres::PoolConfig {
        max_size: 4,
        ..Default::default()
    });

    pg_config
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .expect("Failed to create test connection pool")
}
