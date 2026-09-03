use anyhow::{Context, Result};
use database::Database;
use sha2::{Digest, Sha256};
use std::env;
use std::time::Duration;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use tracing::{debug, info, warn};

static SHARED_DB_READY: OnceCell<()> = OnceCell::const_new();

/// Set by the nextest setup script after the shared database has been prepared.
/// Each nextest test runs in its own process, so an in-process `OnceCell` alone
/// cannot prevent every process from repeating database creation and migrations.
pub const E2E_DATABASE_BOOTSTRAPPED_ENV: &str = "CLOUD_API_E2E_DATABASE_BOOTSTRAPPED";
pub const MOCK_USER_ID: &str = "11111111-1111-1111-1111-111111111111";

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

pub fn nextest_bootstrap_marker() -> Option<String> {
    if env::var("NEXTEST").ok().as_deref() != Some("1") {
        return None;
    }

    let run_id = env::var("NEXTEST_RUN_ID")
        .expect("NEXTEST_RUN_ID must be set when an e2e test runs under nextest");
    let connection_identity = format!(
        "{run_id}\0{}\0{}\0{}\0{}",
        db_host(),
        db_port(),
        db_user(),
        get_test_db_name()
    );
    Some(hex::encode(Sha256::digest(connection_identity.as_bytes())))
}

fn database_was_prebootstrapped() -> bool {
    let Some(expected_marker) = nextest_bootstrap_marker() else {
        return false;
    };

    match env::var(E2E_DATABASE_BOOTSTRAPPED_ENV) {
        Ok(actual_marker) => {
            assert_eq!(
                actual_marker, expected_marker,
                "the e2e bootstrap marker does not match this nextest run and database target"
            );
            true
        }
        Err(env::VarError::NotPresent) => false,
        Err(env::VarError::NotUnicode(_)) => {
            panic!("{E2E_DATABASE_BOOTSTRAPPED_ENV} must contain a UTF-8 database name")
        }
    }
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

async fn bootstrap_shared_db_once() -> Result<()> {
    let _ = dotenvy::dotenv();
    let db_name = get_test_db_name();
    let host = db_host();
    let port = db_port();
    let user = db_user();
    let password = db_password();

    // Connect to the `postgres` admin database. This connection holds the
    // advisory lock for the entire bootstrap (CREATE DATABASE + migrations).
    let admin_conn_string =
        format!("host={host} port={port} user={user} password={password} dbname=postgres");
    let (client, connection) = tokio_postgres::connect(&admin_conn_string, NoTls)
        .await
        .context("connect to the admin database for e2e bootstrap")?;

    tokio::spawn(async move {
        if let Err(error) = connection.await {
            warn!(%error, "Admin database connection closed during e2e bootstrap");
        }
    });

    // Keep direct `cargo test` invocations safe when more than one test binary
    // starts at once. The lock is released automatically with this connection.
    client
        .execute("SELECT pg_advisory_lock($1)", &[&BOOTSTRAP_LOCK_KEY])
        .await
        .context("acquire the e2e database bootstrap advisory lock")?;

    let database_exists: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
            &[&db_name],
        )
        .await
        .context("check whether the shared e2e database exists")?
        .get(0);

    if database_exists {
        debug!(database = %db_name, "Shared e2e database already exists");
    } else {
        client
            .execute(
                &format!("CREATE DATABASE {}", quote_identifier(&db_name)),
                &[],
            )
            .await
            .with_context(|| format!("create shared e2e database '{db_name}'"))?;
        info!(database = %db_name, "Created shared e2e database");
    }

    // Run migrations while still holding the advisory lock so direct cargo
    // invocations cannot race on refinery's schema history table.
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

    let mut pg_config = deadpool_postgres::Config::new();
    pg_config.host = db_config.host.clone();
    pg_config.port = Some(db_config.port);
    pg_config.dbname = Some(db_config.database.clone());
    pg_config.user = Some(db_config.username.clone());
    pg_config.password = Some(db_config.password.clone());
    pg_config.pool = Some(deadpool_postgres::PoolConfig {
        max_size: 1,
        timeouts: deadpool_postgres::Timeouts {
            wait: Some(Duration::from_secs(10)),
            create: Some(Duration::from_secs(10)),
            recycle: Some(Duration::from_secs(10)),
        },
        ..Default::default()
    });
    let migration_pool = pg_config
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .context("create the e2e migration connection pool")?;
    let database = Database::new(migration_pool.into());
    database
        .run_migrations()
        .await
        .context("run migrations on the shared e2e database")?;
    seed_shared_test_fixtures(&database).await?;

    debug!(database = %db_name, "Shared e2e database is ready");
    Ok(())
}

async fn seed_shared_test_fixtures(database: &Database) -> Result<()> {
    let user_id = uuid::Uuid::parse_str(MOCK_USER_ID).expect("fixed mock user ID must be valid");
    let client = database
        .pool()
        .get()
        .await
        .context("get a connection for shared e2e fixtures")?;

    // Jobs are operational state, not durable test fixtures. A killed test can
    // leave one queued/running, which then conflicts with a later run's unique
    // active-scope index or gets picked up by a recovery test. The setup script
    // runs before nextest launches any E2E process, so this is the safe point to
    // restore an empty queue and keep repeated local runs deterministic.
    client
        .execute("DELETE FROM database_encryption_jobs", &[])
        .await
        .context("clear database encryption jobs from earlier e2e runs")?;

    client
        .execute(
            "INSERT INTO users (
                id, email, username, display_name, avatar_url,
                auth_provider, provider_user_id, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW())
             ON CONFLICT (id) DO UPDATE SET
                email = EXCLUDED.email,
                username = EXCLUDED.username,
                display_name = EXCLUDED.display_name,
                avatar_url = EXCLUDED.avatar_url,
                updated_at = NOW(),
                last_login_at = NULL,
                is_active = TRUE,
                auth_provider = EXCLUDED.auth_provider,
                provider_user_id = EXCLUDED.provider_user_id,
                tokens_revoked_at = NULL",
            &[
                &user_id,
                &"admin@test.com",
                &"testuser",
                &Some("Test User".to_string()),
                &Some("https://example.com/avatar.jpg".to_string()),
                &"mock",
                &"mock_123",
            ],
        )
        .await
        .context("seed the shared e2e mock user")?;

    let seeded_user = client
        .query_one(
            "SELECT email, username, auth_provider, provider_user_id,
                    is_active, tokens_revoked_at IS NULL
             FROM users WHERE id = $1",
            &[&user_id],
        )
        .await
        .context("verify the shared e2e mock user")?;
    anyhow::ensure!(
        seeded_user.get::<_, String>(0) == "admin@test.com"
            && seeded_user.get::<_, String>(1) == "testuser"
            && seeded_user.get::<_, String>(2) == "mock"
            && seeded_user.get::<_, String>(3) == "mock_123"
            && seeded_user.get::<_, bool>(4)
            && seeded_user.get::<_, bool>(5),
        "the shared e2e mock user was not restored to its deterministic state"
    );

    Ok(())
}

async fn bootstrap_shared_db_with_retry() -> Result<()> {
    const MAX_ATTEMPTS: usize = 3;

    let mut last_error = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match bootstrap_shared_db_once().await {
            Ok(()) => return Ok(()),
            Err(error) => {
                warn!(attempt, max_attempts = MAX_ATTEMPTS, %error, "E2E database bootstrap attempt failed");
                last_error = Some(error);
                if attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(250 * attempt as u64)).await;
                }
            }
        }
    }

    Err(last_error.expect("at least one e2e database bootstrap attempt must run"))
}

/// Bootstrap the shared database once: create it if missing and run migrations.
///
/// The nextest setup script calls this before launching any e2e test processes.
/// `OnceCell` and the PostgreSQL advisory lock preserve compatibility with direct
/// `cargo test` invocations outside nextest.
async fn ensure_shared_db() {
    SHARED_DB_READY
        .get_or_init(|| async {
            bootstrap_shared_db_with_retry()
                .await
                .expect("Failed to bootstrap the shared e2e database after 3 attempts");
        })
        .await;
}

/// Entry point used by the nextest setup target.
pub async fn bootstrap_test_database() {
    ensure_shared_db().await;
}

/// Create a 4-connection deadpool pool to the shared e2e database.
/// Called once per test.
pub async fn create_test_pool() -> database::pool::DbPool {
    let _ = dotenvy::dotenv();
    if !database_was_prebootstrapped() {
        ensure_shared_db().await;
    }

    let mut pg_config = deadpool_postgres::Config::new();
    pg_config.host = Some(db_host());
    pg_config.port = Some(db_port());
    pg_config.dbname = Some(get_test_db_name());
    pg_config.user = Some(db_user());
    pg_config.password = Some(db_password());
    pg_config.application_name = Some(format!("cloud-api-e2e-{}", uuid::Uuid::new_v4().simple()));

    pg_config.pool = Some(deadpool_postgres::PoolConfig {
        max_size: 4,
        timeouts: deadpool_postgres::Timeouts {
            wait: Some(Duration::from_secs(10)),
            create: Some(Duration::from_secs(10)),
            recycle: Some(Duration::from_secs(10)),
        },
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
