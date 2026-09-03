use anyhow::{Context, Result};
use database::Database;
use sha2::{Digest, Sha256};
use std::env;
use std::time::Duration;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use tracing::{debug, info, warn};

use super::{
    E2E_DEEPSEEK_MODEL_NAME, E2E_GLM_MODEL_NAME, E2E_PRIVACY_FILTER_MODEL_NAME,
    E2E_QWEN_CACHE_MODEL_NAME, E2E_QWEN_CACHE_READ_COST_WITH_CACHE, E2E_QWEN_IMAGE_MODEL_NAME,
    E2E_QWEN_INPUT_COST_PER_TOKEN, E2E_QWEN_MODEL_NAME, E2E_QWEN_OMNI_MODEL_NAME,
    E2E_QWEN_OUTPUT_COST_PER_TOKEN, E2E_QWEN_RERANKER_MODEL_NAME,
};

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

#[derive(Clone, Copy)]
struct SharedModelFixture {
    model_name: &'static str,
    display_name: &'static str,
    description: &'static str,
    input_cost_per_token: i64,
    output_cost_per_token: i64,
    cost_per_image: i64,
    cache_read_cost_per_token: Option<i64>,
    context_length: i32,
    max_output_length: i32,
    verifiable: bool,
    allow_free: bool,
}

const SHARED_MODEL_FIXTURES: [SharedModelFixture; 8] = [
    SharedModelFixture {
        model_name: E2E_QWEN_MODEL_NAME,
        display_name: "E2E Qwen fixture",
        description: "Deterministic E2E model fixture",
        input_cost_per_token: E2E_QWEN_INPUT_COST_PER_TOKEN,
        output_cost_per_token: E2E_QWEN_OUTPUT_COST_PER_TOKEN,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        context_length: 128_000,
        max_output_length: 1_024,
        verifiable: true,
        allow_free: false,
    },
    SharedModelFixture {
        model_name: E2E_QWEN_CACHE_MODEL_NAME,
        display_name: "E2E Qwen fixture",
        description: "Deterministic E2E model fixture",
        input_cost_per_token: E2E_QWEN_INPUT_COST_PER_TOKEN,
        output_cost_per_token: E2E_QWEN_OUTPUT_COST_PER_TOKEN,
        cost_per_image: 0,
        cache_read_cost_per_token: Some(E2E_QWEN_CACHE_READ_COST_WITH_CACHE),
        context_length: 128_000,
        max_output_length: 1_024,
        verifiable: true,
        allow_free: false,
    },
    SharedModelFixture {
        model_name: E2E_PRIVACY_FILTER_MODEL_NAME,
        display_name: "Privacy Filter",
        description: "PII span detection (token classification)",
        input_cost_per_token: 1_000_000,
        output_cost_per_token: 0,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        context_length: 512,
        max_output_length: 1_024,
        verifiable: false,
        allow_free: true,
    },
    SharedModelFixture {
        model_name: E2E_GLM_MODEL_NAME,
        display_name: "GLM-4.6",
        description: "GLM 4.6 model for testing",
        input_cost_per_token: 1_000_000,
        output_cost_per_token: 2_000_000,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        context_length: 128_000,
        max_output_length: 1_024,
        verifiable: true,
        allow_free: false,
    },
    SharedModelFixture {
        model_name: E2E_DEEPSEEK_MODEL_NAME,
        display_name: "DeepSeek V3.1",
        description: "DeepSeek V3.1 model with encryption support",
        input_cost_per_token: 1_000_000,
        output_cost_per_token: 2_000_000,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        context_length: 128_000,
        max_output_length: 1_024,
        verifiable: true,
        allow_free: false,
    },
    SharedModelFixture {
        model_name: E2E_QWEN_OMNI_MODEL_NAME,
        display_name: "Qwen3-Omni 30B",
        description: "Qwen3-Omni model with audio input/output support",
        input_cost_per_token: 1_500_000,
        output_cost_per_token: 3_000_000,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        context_length: 128_000,
        max_output_length: 1_024,
        verifiable: true,
        allow_free: false,
    },
    SharedModelFixture {
        model_name: E2E_QWEN_IMAGE_MODEL_NAME,
        display_name: "Qwen-Image",
        description: "Qwen Image generation model",
        input_cost_per_token: 0,
        output_cost_per_token: 0,
        cost_per_image: 40_000_000,
        cache_read_cost_per_token: None,
        context_length: 4_096,
        max_output_length: 1_024,
        verifiable: true,
        allow_free: false,
    },
    SharedModelFixture {
        model_name: E2E_QWEN_RERANKER_MODEL_NAME,
        display_name: "Qwen3 Reranker",
        description: "Qwen3 document reranking model",
        input_cost_per_token: 1_000_000,
        output_cost_per_token: 0,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        context_length: 32_768,
        max_output_length: 1_024,
        verifiable: false,
        allow_free: false,
    },
];

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

    // These high-traffic catalog fixtures are immutable during ordinary E2E
    // tests. Seed them once before nextest starts instead of making every test
    // PATCH the same model row. Tests that exercise alternate model metadata
    // use separate names so they cannot contaminate these baselines.
    for fixture in SHARED_MODEL_FIXTURES {
        let model_name = fixture.model_name.to_string();
        let display_name = fixture.display_name.to_string();
        let description = fixture.description.to_string();
        let seeded_model = client
            .query_one(
                "INSERT INTO models (
                    model_name, model_display_name, model_description,
                    input_cost_per_token, output_cost_per_token, cost_per_image,
                    cache_read_cost_per_token, text_pricing, context_length,
                    max_output_length, verifiable, is_active, allow_free, owned_by,
                    provider_type, provider_config, attestation_supported,
                    input_modalities, output_modalities, inference_url,
                    hugging_face_id, quantization, supported_sampling_parameters,
                    supported_features, datacenters, is_ready, deprecation_date,
                    openrouter_slug, attestation_policy
                 ) VALUES (
                    $1, $2, $3, $4, $5, $6, $7, NULL, $8, $9, $10, TRUE, $11,
                    'nearai', 'vllm', NULL, TRUE, NULL, NULL, NULL, NULL, NULL,
                    ARRAY[]::TEXT[], ARRAY[]::TEXT[], NULL, NULL, NULL, NULL,
                    'near_only'
                 )
                 ON CONFLICT (model_name) DO UPDATE SET
                    model_display_name = EXCLUDED.model_display_name,
                    model_description = EXCLUDED.model_description,
                    input_cost_per_token = EXCLUDED.input_cost_per_token,
                    output_cost_per_token = EXCLUDED.output_cost_per_token,
                    cost_per_image = EXCLUDED.cost_per_image,
                    cache_read_cost_per_token = EXCLUDED.cache_read_cost_per_token,
                    text_pricing = EXCLUDED.text_pricing,
                    context_length = EXCLUDED.context_length,
                    max_output_length = EXCLUDED.max_output_length,
                    verifiable = EXCLUDED.verifiable,
                    is_active = EXCLUDED.is_active,
                    allow_free = EXCLUDED.allow_free,
                    owned_by = EXCLUDED.owned_by,
                    provider_type = EXCLUDED.provider_type,
                    provider_config = EXCLUDED.provider_config,
                    attestation_supported = EXCLUDED.attestation_supported,
                    input_modalities = EXCLUDED.input_modalities,
                    output_modalities = EXCLUDED.output_modalities,
                    inference_url = EXCLUDED.inference_url,
                    hugging_face_id = EXCLUDED.hugging_face_id,
                    quantization = EXCLUDED.quantization,
                    supported_sampling_parameters = EXCLUDED.supported_sampling_parameters,
                    supported_features = EXCLUDED.supported_features,
                    datacenters = EXCLUDED.datacenters,
                    is_ready = EXCLUDED.is_ready,
                    deprecation_date = EXCLUDED.deprecation_date,
                    openrouter_slug = EXCLUDED.openrouter_slug,
                    attestation_policy = EXCLUDED.attestation_policy,
                    updated_at = NOW()
                 RETURNING id",
                &[
                    &model_name,
                    &display_name,
                    &description,
                    &fixture.input_cost_per_token,
                    &fixture.output_cost_per_token,
                    &fixture.cost_per_image,
                    &fixture.cache_read_cost_per_token,
                    &fixture.context_length,
                    &fixture.max_output_length,
                    &fixture.verifiable,
                    &fixture.allow_free,
                ],
            )
            .await
            .with_context(|| format!("seed shared E2E model fixture '{model_name}'"))?;
        let model_id: uuid::Uuid = seeded_model.get(0);
        client
            .execute(
                "DELETE FROM model_aliases WHERE canonical_model_id = $1",
                &[&model_id],
            )
            .await
            .with_context(|| format!("clear aliases for shared E2E model '{model_name}'"))?;
    }

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
    pg_config.connect_timeout = Some(Duration::from_secs(10));
    pg_config.keepalives = Some(true);
    pg_config.keepalives_idle = Some(Duration::from_secs(5));
    pg_config.options = Some(
        "-c statement_timeout=30000 -c lock_timeout=10000 \
         -c idle_in_transaction_session_timeout=30000"
            .to_string(),
    );
    // The E2E runner creates many short-lived processes through Docker's
    // published PostgreSQL port. Verify pooled connections before reuse so a
    // hard-closed socket is discarded instead of failing the test's next SQL.
    pg_config.manager = Some(deadpool_postgres::ManagerConfig {
        recycling_method: deadpool_postgres::RecyclingMethod::Verified,
    });

    pg_config.pool = Some(deadpool_postgres::PoolConfig {
        max_size: 4,
        timeouts: deadpool_postgres::Timeouts {
            wait: Some(Duration::from_secs(10)),
            create: Some(Duration::from_secs(10)),
            recycle: Some(Duration::from_secs(10)),
        },
        ..Default::default()
    });

    // On Linux, bound how long unacknowledged test traffic may remain stuck in
    // the TCP stack. This complements server-side statement/lock timeouts for
    // the Docker-published PostgreSQL path used by CI.
    let mut tokio_pg_config = pg_config
        .get_pg_config()
        .expect("Failed to create test PostgreSQL configuration");
    tokio_pg_config.tcp_user_timeout(Duration::from_secs(10));
    let manager = deadpool_postgres::Manager::from_config(
        tokio_pg_config,
        tokio_postgres::NoTls,
        pg_config.get_manager_config(),
    );

    deadpool_postgres::Pool::builder(manager)
        .config(pg_config.get_pool_config())
        .runtime(deadpool_postgres::Runtime::Tokio1)
        .build()
        .expect("Failed to create test connection pool")
        .into()
}
