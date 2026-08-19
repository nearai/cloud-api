use database::Database;
use sha2::{Digest, Sha256};
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tokio_postgres::{Client, NoTls};
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

/// Fixed key for the PostgreSQL advisory lock that serializes DB bootstrap
/// across test binaries. Chosen arbitrarily, just needs to be consistent.
const BOOTSTRAP_LOCK_KEY: i64 = 0x0e2e_b007_57a9;
const BOOTSTRAP_MARKER_TABLE: &str = "_e2e_bootstrap_status";
const BOOTSTRAP_MARKER_KEY: &str = "migrations_sha256";

fn migrations_path() -> PathBuf {
    let env_path = env::var("DATABASE_MIGRATIONS_PATH").ok().map(PathBuf::from);
    let relative_path = std::env::current_dir()
        .expect("Failed to get current directory")
        .join("crates/database/src/migrations/sql");
    let compile_time_path = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../database/src/migrations/sql"
    ));

    env_path
        .into_iter()
        .chain([relative_path, compile_time_path])
        .find(|path| path.exists())
        .expect("Migrations folder not found for e2e bootstrap fingerprint")
}

fn migrations_fingerprint() -> String {
    let mut migration_files = std::fs::read_dir(migrations_path())
        .expect("Failed to read migrations directory")
        .map(|entry| entry.expect("Failed to read migration file entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "sql"))
        .collect::<Vec<_>>();
    migration_files.sort();

    let mut hasher = Sha256::new();
    for path in migration_files {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("Migration filename should be valid UTF-8");
        let contents = std::fs::read(&path).expect("Failed to read migration file");
        hasher.update(file_name.as_bytes());
        hasher.update([0]);
        hasher.update(contents);
        hasher.update([0]);
    }

    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn connect_postgres(conn_string: &str, purpose: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(conn_string, NoTls)
        .await
        .unwrap_or_else(|error| panic!("Failed to connect to {purpose}: {error}"));

    let purpose = purpose.to_string();
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("PostgreSQL connection error during {purpose}: {e}");
        }
    });

    client
}

async fn migrations_already_bootstrapped(client: &Client, fingerprint: &str) -> bool {
    client
        .batch_execute(&format!(
            "CREATE TABLE IF NOT EXISTS {BOOTSTRAP_MARKER_TABLE} (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                ready_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )"
        ))
        .await
        .expect("Failed to create e2e bootstrap marker table");

    client
        .query_opt(
            &format!("SELECT value FROM {BOOTSTRAP_MARKER_TABLE} WHERE key = $1"),
            &[&BOOTSTRAP_MARKER_KEY],
        )
        .await
        .expect("Failed to read e2e bootstrap marker")
        .map(|row| row.get::<_, String>("value") == fingerprint)
        .unwrap_or(false)
}

async fn mark_migrations_bootstrapped(client: &Client, fingerprint: &str) {
    client
        .execute(
            &format!(
                "INSERT INTO {BOOTSTRAP_MARKER_TABLE} (key, value, ready_at)
                 VALUES ($1, $2, NOW())
                 ON CONFLICT (key) DO UPDATE
                 SET value = EXCLUDED.value,
                     ready_at = EXCLUDED.ready_at"
            ),
            &[&BOOTSTRAP_MARKER_KEY, &fingerprint],
        )
        .await
        .expect("Failed to write e2e bootstrap marker");
}

/// Bootstrap the shared database once: create it if missing, run migrations, drop the bootstrap pool.
///
/// OnceCell gates within a single binary (process), while a PostgreSQL advisory
/// lock serializes across the multiple test binaries that nextest launches in
/// parallel. The lock is automatically released when the admin connection drops.
async fn ensure_shared_db() {
    SHARED_DB_READY
        .get_or_init(|| async {
            let db_name = get_test_db_name();
            let host = db_host();
            let port = db_port();
            let user = db_user();
            let password = db_password();

            // Connect to the `postgres` admin database. This connection holds the
            // advisory lock for the entire bootstrap (CREATE DATABASE + migrations).
            let admin_conn_string =
                format!("host={host} port={port} user={user} password={password} dbname=postgres");
            let client = connect_postgres(&admin_conn_string, "admin database for bootstrap").await;

            // Serialize bootstrap across test binaries. pg_advisory_lock blocks
            // until the lock is available; it's released when the session ends.
            client
                .execute("SELECT pg_advisory_lock($1)", &[&BOOTSTRAP_LOCK_KEY])
                .await
                .expect("Failed to acquire advisory lock for e2e bootstrap");

            // CREATE DATABASE inside the lock, swallow errors (another binary may
            // have already created it in a previous lock holder's session).
            match client
                .execute(&format!("CREATE DATABASE {db_name}"), &[])
                .await
            {
                Ok(_) => info!("Created shared e2e database '{db_name}'"),
                Err(e) => {
                    debug!("CREATE DATABASE {db_name} returned error (likely already exists): {e}");
                }
            }

            let fingerprint = migrations_fingerprint();
            let target_conn_string =
                format!("host={host} port={port} user={user} password={password} dbname={db_name}");
            let target_client =
                connect_postgres(&target_conn_string, "shared e2e database marker").await;
            if migrations_already_bootstrapped(&target_client, &fingerprint).await {
                debug!("Shared e2e database '{db_name}' already has current migrations");
                drop(target_client);
                drop(client);
                return;
            }
            drop(target_client);

            // Run migrations while still holding the advisory lock so refinery's
            // schema_history table creation doesn't race across binaries. A
            // fingerprint marker prevents later test processes from rerunning
            // refinery while the shared database is already in active use.
            let db_config = config::DatabaseConfig {
                primary_app_id: "postgres-test".to_string(),
                gateway_subdomain: "cvm1.near.ai".to_string(),
                port,
                host: Some(host.clone()),
                database: db_name.clone(),
                username: user.clone(),
                password: password.clone(),
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

            let target_client =
                connect_postgres(&target_conn_string, "shared e2e database marker").await;
            mark_migrations_bootstrapped(&target_client, &fingerprint).await;
            drop(target_client);

            debug!("Shared e2e database '{db_name}' ready with migrations");
            drop(database);

            // Dropping `client` closes the session which releases the advisory lock.
            drop(client);
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
    pg_config.application_name = Some(format!("cloud-api-e2e-{}", uuid::Uuid::new_v4().simple()));

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
        .into()
}
