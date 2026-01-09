#![allow(dead_code)]
//! Test utilities for e2e tests with isolated database per test.
//!
//! Uses template database pattern: migrations run once on template,
//! each test clones from template for isolation and speed.

pub mod db_setup;

use api::{build_app_with_config, init_auth_services, models::BatchUpdateModelApiRequest};
use base64::Engine;
use chrono::Utc;
use config::ApiConfig;
use database::Database;
pub use services::auth::ports::MOCK_USER_AGENT;
use services::auth::AccessTokenClaims;
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[cfg(test)]
use ed25519_dalek::{Signature as Ed25519Signature, VerifyingKey as Ed25519VerifyingKey};
#[cfg(test)]
use k256::ecdsa::{RecoveryId, Signature as EcdsaSignature, VerifyingKey};
#[cfg(test)]
use sha3::Keccak256;

pub const MOCK_USER_ID: &str = "11111111-1111-1111-1111-111111111111";

/// RAII guard for test database cleanup.
pub struct TestDatabaseGuard {
    db_name: String,
    db_config: config::DatabaseConfig,
}

impl Drop for TestDatabaseGuard {
    fn drop(&mut self) {
        let db_name = self.db_name.clone();
        let config = self.db_config.clone();

        if db_name.is_empty() {
            return;
        }

        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                if let Err(e) = db_setup::drop_test_database(&config, &db_name).await {
                    eprintln!("Warning: Failed to drop test database '{}': {}", db_name, e);
                }
            });
        });

        if handle.join().is_err() {
            eprintln!("Warning: Database cleanup thread panicked");
        }
    }
}

pub fn test_config_with_db(db_name: &str) -> ApiConfig {
    let _ = dotenvy::dotenv();
    ApiConfig {
        server: config::ServerConfig {
            host: std::env::var("SERVER_HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
            port: std::env::var("SERVER_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(0), // Use port 0 to get a random available port
        },
        model_discovery: config::ModelDiscoveryConfig {
            discovery_server_url: std::env::var("MODEL_DISCOVERY_SERVER_URL")
                .unwrap_or_else(|_| "http://localhost:8080/models".to_string()),
            api_key: std::env::var("MODEL_DISCOVERY_API_KEY")
                .ok()
                .or(Some("test_api_key".to_string())),
            refresh_interval: std::env::var("MODEL_DISCOVERY_REFRESH_INTERVAL")
                .ok()
                .and_then(|i| i.parse().ok())
                .unwrap_or(3600), // 1 hour - large value to avoid refresh during tests
            timeout: std::env::var("MODEL_DISCOVERY_TIMEOUT")
                .ok()
                .and_then(|t| t.parse().ok())
                .unwrap_or(5),
            inference_timeout: std::env::var("MODEL_INFERENCE_TIMEOUT")
                .ok()
                .and_then(|t| t.parse().ok())
                .unwrap_or(30 * 60), // 30 minutes
        },
        logging: config::LoggingConfig {
            level: "debug".to_string(),
            format: "compact".to_string(),
            modules: std::collections::HashMap::new(),
        },
        dstack_client: config::DstackClientConfig {
            url: std::env::var("DSTACK_CLIENT_URL")
                .unwrap_or_else(|_| "http://localhost:8000".to_string()),
        },
        auth: config::AuthConfig {
            mock: true,
            encoding_key: "mock_encoding_key".to_string(),
            github: None,
            google: None,
            near: config::NearConfig::default(),
            admin_domains: vec!["test.com".to_string()],
        },
        database: db_config_for_tests_with_name(db_name),
        s3: config::S3Config {
            mock: true,
            bucket: std::env::var("AWS_S3_BUCKET").unwrap_or_else(|_| "test-bucket".to_string()),
            region: std::env::var("AWS_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string()),
            encryption_key: std::env::var("S3_ENCRYPTION_KEY").unwrap_or_else(|_| {
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string()
            }),
        },
        otlp: config::OtlpConfig {
            endpoint: std::env::var("TELEMETRY_OTLP_ENDPOINT")
                .unwrap_or_else(|_| "http://localhost:4317".to_string()),
            protocol: std::env::var("TELEMETRY_OTLP_PROTOCOL").unwrap_or("grpc".to_string()),
        },
        cors: config::CorsConfig::default(),
    }
}

pub fn test_config() -> ApiConfig {
    test_config_with_db(&db_setup::get_test_db_name())
}

fn db_config_for_tests_with_name(db_name: &str) -> config::DatabaseConfig {
    config::DatabaseConfig {
        primary_app_id: "postgres-test".to_string(),
        gateway_subdomain: "cvm1.near.ai".to_string(),
        port: 5432,
        host: None,
        database: db_name.to_string(),
        username: std::env::var("DATABASE_USERNAME").unwrap_or("postgres".to_string()),
        password: std::env::var("DATABASE_PASSWORD").unwrap_or("postgres".to_string()),
        max_connections: 2,
        tls_enabled: false,
        tls_ca_cert_path: None,
        refresh_interval: 30,
        mock: false,
    }
}

fn db_config_for_tests() -> config::DatabaseConfig {
    db_config_for_tests_with_name(&db_setup::get_test_db_name())
}

pub fn get_session_id() -> String {
    format!("rt_{MOCK_USER_ID}")
}

pub async fn get_access_token_from_refresh_token(
    server: &axum_test::TestServer,
    refresh_token: String,
) -> String {
    let response = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", format!("Bearer {refresh_token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Failed to refresh access token"
    );

    let refresh_response = response.json::<api::models::AccessAndRefreshTokenResponse>();
    refresh_response.access_token
}

async fn init_database_connection(config: &config::DatabaseConfig) -> Arc<Database> {
    Arc::new(
        Database::from_config(config)
            .await
            .expect("Failed to connect to database"),
    )
}

async fn build_test_server_components(
    database: Arc<Database>,
    config: ApiConfig,
) -> (
    axum_test::TestServer,
    Arc<services::inference_provider_pool::InferenceProviderPool>,
    Arc<inference_providers::mock::MockProvider>,
) {
    // Create mock user in database for foreign key constraints
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);

    // Use mock inference providers instead of real VLLM to avoid flakiness
    let (inference_provider_pool, mock_provider) =
        api::init_inference_providers_with_mocks(&config).await;
    let metrics_service = Arc::new(services::metrics::MockMetricsService);
    let domain_services = api::init_domain_services_with_pool(
        database.clone(),
        &config,
        auth_components.organization_service.clone(),
        inference_provider_pool.clone(),
        metrics_service,
    )
    .await;

    let app = build_app_with_config(
        database.clone(),
        auth_components,
        domain_services,
        Arc::new(config),
    );
    let server = axum_test::TestServer::new(app).unwrap();

    (server, inference_provider_pool, mock_provider)
}

/// Setup test server. Returns guard that must be held to ensure database cleanup.
pub async fn setup_test_server() -> (axum_test::TestServer, TestDatabaseGuard) {
    let (server, _, _, _, guard) = setup_test_server_with_pool().await;
    (server, guard)
}

/// Setup test server with database access. Returns guard that must be held to ensure cleanup.
pub async fn setup_test_server_with_database(
) -> (axum_test::TestServer, Arc<Database>, TestDatabaseGuard) {
    let (server, _, _, database, guard) = setup_test_server_with_pool().await;
    (server, database, guard)
}

/// Setup test server with all components. Returns guard that must be held to ensure cleanup.
pub async fn setup_test_server_with_pool() -> (
    axum_test::TestServer,
    std::sync::Arc<services::inference_provider_pool::InferenceProviderPool>,
    std::sync::Arc<inference_providers::mock::MockProvider>,
    Arc<Database>,
    TestDatabaseGuard,
) {
    let infra = setup_test_infrastructure().await;

    let (server, inference_provider_pool, mock_provider) =
        build_test_server_components(infra.database.clone(), infra.config).await;

    (
        server,
        inference_provider_pool,
        mock_provider,
        infra.database,
        infra.guard,
    )
}

/// Common test infrastructure setup (database, config, guard)
struct TestInfrastructure {
    database: Arc<Database>,
    config: config::ApiConfig,
    guard: TestDatabaseGuard,
}

/// Initialize common test infrastructure (tracing, database, config)
async fn setup_test_infrastructure() -> TestInfrastructure {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::level_filters::LevelFilter::DEBUG)
        .try_init();

    // Generate a unique test ID for this test's database
    let test_id = uuid::Uuid::new_v4().to_string();

    // Get base config for database connection info
    let base_db_config = db_config_for_tests();

    // Create a unique database from the template
    let db_name = db_setup::create_test_database_from_template(&base_db_config, &test_id)
        .await
        .expect("Failed to create test database from template");

    // Create config with the new database name
    let config = test_config_with_db(&db_name);
    let db_config = config.database.clone();

    // Connect to the new test database
    let database = init_database_connection(&config.database).await;

    let guard = TestDatabaseGuard { db_name, db_config };

    TestInfrastructure {
        database,
        config,
        guard,
    }
}

/// Setup test server with MCP client factory injection.
pub async fn setup_test_server_with_mcp_factory(
    mcp_client_factory: std::sync::Arc<dyn services::responses::tools::McpClientFactory>,
) -> (
    axum_test::TestServer,
    std::sync::Arc<services::inference_provider_pool::InferenceProviderPool>,
    std::sync::Arc<inference_providers::mock::MockProvider>,
    TestDatabaseGuard,
) {
    let infra = setup_test_infrastructure().await;

    // Create mock user in database for foreign key constraints
    assert_mock_user_in_db(&infra.database).await;

    let auth_components = init_auth_services(infra.database.clone(), &infra.config);

    // Use mock inference providers
    let (inference_provider_pool, mock_provider) =
        api::init_inference_providers_with_mocks(&infra.config).await;
    let metrics_service = Arc::new(services::metrics::MockMetricsService);

    // Initialize domain services with MCP factory
    let domain_services = api::init_domain_services_with_mcp_factory(
        infra.database.clone(),
        &infra.config,
        auth_components.organization_service.clone(),
        inference_provider_pool.clone(),
        metrics_service,
        mcp_client_factory,
    )
    .await;

    let app = build_app_with_config(
        infra.database.clone(),
        auth_components,
        domain_services,
        Arc::new(infra.config),
    );
    let server = axum_test::TestServer::new(app).unwrap();

    (server, inference_provider_pool, mock_provider, infra.guard)
}

pub async fn setup_unique_test_session(database: &Arc<Database>) -> (String, String) {
    let user_id = uuid::Uuid::new_v4();
    let user_id_str = user_id.to_string();
    // Session ID format: rt_{uuid} (with dashes so it can be parsed by MockAuthService)
    let session_id = format!("rt_{user_id_str}");
    let email = format!("test-{user_id_str}@test.com");

    let pool = database.pool();
    let client = pool.get().await.expect("Failed to get database connection");
    let _ = client.execute(
        "INSERT INTO users (id, email, username, display_name, avatar_url, auth_provider, provider_user_id, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW())
         ON CONFLICT (id) DO NOTHING",
        &[
            &user_id,
            &email,
            &format!("testuser-{user_id_str}"),
            &Some("Test User".to_string()),
            &Some("https://example.com/avatar.jpg".to_string()),
            &"mock",
            &"mock_user",
        ],
    ).await;

    (session_id, email)
}

pub async fn assert_mock_user_in_db(database: &Arc<Database>) {
    let pool = database.pool();
    let client = pool.get().await.expect("Failed to get database connection");

    let _ = client.execute(
        "INSERT INTO users (id, email, username, display_name, avatar_url, auth_provider, provider_user_id, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW())
         ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email",
        &[
            &uuid::Uuid::parse_str(MOCK_USER_ID).unwrap(),
            &"admin@test.com", // Using test.com domain for admin access
            &"testuser",
            &Some("Test User".to_string()),
            &Some("https://example.com/avatar.jpg".to_string()),
            &"mock",
            &"mock_123",
        ],
    ).await.expect("Failed to create mock user");

    tracing::debug!("Mock user created/exists in database: {}", MOCK_USER_ID);
}

pub async fn create_org(server: &axum_test::TestServer) -> api::models::OrganizationResponse {
    create_org_with_session(server, &get_session_id()).await
}

pub async fn create_org_with_session(
    server: &axum_test::TestServer,
    session_id: &str,
) -> api::models::OrganizationResponse {
    let request = api::models::CreateOrganizationRequest {
        name: uuid::Uuid::new_v4().to_string(),
        description: Some("A test organization".to_string()),
    };
    let response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!(request))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::OrganizationResponse>()
}

pub async fn setup_org_with_credits(
    server: &axum_test::TestServer,
    amount_nano_dollars: i64,
) -> api::models::OrganizationResponse {
    setup_org_with_credits_and_session(server, amount_nano_dollars, &get_session_id()).await
}

pub async fn setup_org_with_credits_and_session(
    server: &axum_test::TestServer,
    amount_nano_dollars: i64,
    session_id: &str,
) -> api::models::OrganizationResponse {
    setup_org_with_credits_and_session_and_email(
        server,
        amount_nano_dollars,
        session_id,
        "admin@test.com",
    )
    .await
}

pub async fn setup_org_with_credits_and_session_and_email(
    server: &axum_test::TestServer,
    amount_nano_dollars: i64,
    session_id: &str,
    email: &str,
) -> api::models::OrganizationResponse {
    let org = create_org_with_session(server, session_id).await;

    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": amount_nano_dollars,
            "currency": "USD"
        },
        "changedBy": email,
        "changeReason": "Test credits"
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&update_request)
        .await;

    assert_eq!(response.status_code(), 200, "Failed to set credits");
    org
}

pub async fn list_workspaces(
    server: &axum_test::TestServer,
    org_id: String,
) -> Vec<api::routes::workspaces::WorkspaceResponse> {
    list_workspaces_with_session(server, org_id, &get_session_id()).await
}

pub async fn list_workspaces_with_session(
    server: &axum_test::TestServer,
    org_id: String,
    session_id: &str,
) -> Vec<api::routes::workspaces::WorkspaceResponse> {
    let response = server
        .get(format!("/v1/organizations/{org_id}/workspaces").as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200);
    let list_response = response.json::<api::routes::workspaces::ListWorkspacesResponse>();
    list_response.workspaces
}

pub async fn create_api_key_in_workspace(
    server: &axum_test::TestServer,
    workspace_id: String,
    name: String,
) -> api::models::ApiKeyResponse {
    create_api_key_in_workspace_with_session(server, workspace_id, name, &get_session_id()).await
}

pub async fn create_api_key_in_workspace_with_session(
    server: &axum_test::TestServer,
    workspace_id: String,
    name: String,
    session_id: &str,
) -> api::models::ApiKeyResponse {
    let request = api::models::CreateApiKeyRequest {
        name,
        expires_at: Some(Utc::now() + chrono::Duration::days(90)),
        spend_limit: None,
    };
    let response = server
        .post(format!("/v1/workspaces/{workspace_id}/api-keys").as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!(request))
        .await;
    assert_eq!(response.status_code(), 201);
    response.json::<api::models::ApiKeyResponse>()
}

pub async fn get_api_key_for_org(server: &axum_test::TestServer, org_id: String) -> String {
    get_api_key_for_org_with_session(server, org_id, &get_session_id()).await
}

pub async fn get_api_key_for_org_with_session(
    server: &axum_test::TestServer,
    org_id: String,
    session_id: &str,
) -> String {
    let workspaces = list_workspaces_with_session(server, org_id, session_id).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp = create_api_key_in_workspace_with_session(
        server,
        workspace.id.clone(),
        "Test API Key".to_string(),
        session_id,
    )
    .await;
    api_key_resp.key.unwrap()
}

pub async fn create_org_and_api_key(
    server: &axum_test::TestServer,
) -> (String, api::models::ApiKeyResponse) {
    let org = create_org(server).await;
    let workspaces = list_workspaces(server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp =
        create_api_key_in_workspace(server, workspace.id.clone(), "Test API Key".to_string()).await;
    (api_key_resp.key.clone().unwrap(), api_key_resp)
}

pub async fn setup_qwen_model(server: &axum_test::TestServer) -> String {
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,
                "currency": "USD"
            },
            "modelDisplayName": "Updated Model Name",
            "modelDescription": "Updated model description",
            "contextLength": 128000,
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );
    let updated = admin_batch_upsert_models(server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "Should have updated 1 model");
    assert_eq!(
        updated[0].input_cost_per_token.amount, 1000000,
        "Input cost per token should be 1000000"
    );
    assert_eq!(
        updated[0].output_cost_per_token.amount, 2000000,
        "Output cost per token should be 2000000"
    );
    // Ensure mock provider registers model before test proceeds
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string()
}

pub async fn setup_glm_model(server: &axum_test::TestServer) -> String {
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "zai-org/GLM-4.6".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,
                "currency": "USD"
            },
            "modelDisplayName": "GLM-4.6",
            "modelDescription": "GLM 4.6 model for testing",
            "contextLength": 128000,
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(server, batch, get_session_id()).await;
    "zai-org/GLM-4.6".to_string()
}

pub async fn setup_deepseek_model(server: &axum_test::TestServer) -> String {
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "deepseek-ai/DeepSeek-V3.1".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,
                "currency": "USD"
            },
            "modelDisplayName": "DeepSeek V3.1",
            "modelDescription": "DeepSeek V3.1 model with encryption support",
            "contextLength": 128000,
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );
    let updated = admin_batch_upsert_models(server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "Should have updated 1 model");
    // Ensure mock provider registers model before test proceeds
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    "deepseek-ai/DeepSeek-V3.1".to_string()
}

pub async fn admin_batch_upsert_models(
    server: &axum_test::TestServer,
    models: BatchUpdateModelApiRequest,
    session_id: String,
) -> Vec<api::models::ModelWithPricing> {
    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&models)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Admin batch upsert should succeed"
    );
    response.json::<Vec<api::models::ModelWithPricing>>()
}

pub async fn list_models(
    server: &axum_test::TestServer,
    api_key: String,
) -> api::models::ModelsResponse {
    let response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::ModelsResponse>()
}

pub fn compute_sha256(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Verify ECDSA signature with Ethereum signed message format.
#[cfg(test)]
pub fn verify_ecdsa_signature(
    signature_text: &str,
    signature_hex: &str,
    signing_address_hex: &str,
) -> bool {
    // Remove 0x prefix if present
    let sig_clean = signature_hex.strip_prefix("0x").unwrap_or(signature_hex);
    let addr_clean = signing_address_hex
        .strip_prefix("0x")
        .unwrap_or(signing_address_hex);

    // Decode signature (should be 65 bytes = 130 hex chars)
    let signature_bytes = match hex::decode(sig_clean) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };

    if signature_bytes.len() != 65 {
        eprintln!(
            "Invalid signature length: expected 65 bytes, got {} bytes",
            signature_bytes.len()
        );
        return false;
    }

    // Extract r, s, and Ethereum v (recovery ID)
    let r_s: [u8; 64] = signature_bytes[..64]
        .try_into()
        .expect("Signature should be 64 bytes for r||s");
    let ethereum_v = signature_bytes[64]; // Last byte: Ethereum v (27 or 28)

    // Validate Ethereum v format
    if ethereum_v != 27 && ethereum_v != 28 {
        eprintln!("Invalid Ethereum v: expected 27 or 28, got {ethereum_v}");
        return false;
    }

    // Parse the signature
    let signature = match EcdsaSignature::from_bytes(&r_s.into()) {
        Ok(sig) => sig,
        Err(e) => {
            eprintln!("Failed to parse signature: {e}");
            return false;
        }
    };

    // Convert Ethereum v (27-28) to k256 RecoveryId (0-3)
    // Ethereum v = 27 + recovery_bit, so recovery_bit = v - 27
    let recovery_bit = ethereum_v - 27;
    let recovery_id = match RecoveryId::try_from(recovery_bit) {
        Ok(rid) => rid,
        Err(e) => {
            eprintln!("Invalid recovery ID: {e}");
            return false;
        }
    };

    // Hash the message with Ethereum signed message format (matching the signing process)
    // Format: \x19Ethereum Signed Message:\n{length}{message}
    let message_bytes = signature_text.as_bytes();
    let prefix = format!("\x19Ethereum Signed Message:\n{}", message_bytes.len());
    let prefix_bytes = prefix.as_bytes();

    // Concatenate prefix + message
    let mut prefixed_message = Vec::with_capacity(prefix_bytes.len() + message_bytes.len());
    prefixed_message.extend_from_slice(prefix_bytes);
    prefixed_message.extend_from_slice(message_bytes);

    // Hash with Keccak256
    let mut hasher = Keccak256::new();
    hasher.update(&prefixed_message);
    let message_hash = hasher.finalize();

    // Recover the public key from the signature
    let recovered_key =
        match VerifyingKey::recover_from_prehash(&message_hash, &signature, recovery_id) {
            Ok(key) => key,
            Err(e) => {
                eprintln!("Failed to recover public key: {e}");
                return false;
            }
        };

    // Convert recovered public key to Ethereum address
    // Get uncompressed public key (65 bytes: 0x04 + 32 bytes x + 32 bytes y)
    let encoded_point = recovered_key.to_encoded_point(false);
    let point_bytes = encoded_point.as_bytes();

    // Extract x and y coordinates (skip the 0x04 prefix, take 64 bytes)
    let uncompressed_pubkey = &point_bytes[1..65];

    // Hash with Keccak256
    let addr_hash = Keccak256::digest(uncompressed_pubkey);

    // Ethereum address is the last 20 bytes (bytes 12..32)
    let recovered_address_bytes = &addr_hash[12..32];
    let recovered_address_hex = hex::encode(recovered_address_bytes);

    // Compare with the expected signing address (should be Ethereum address format)
    let addresses_match = recovered_address_hex.eq_ignore_ascii_case(addr_clean);

    if !addresses_match {
        eprintln!(
            "Address mismatch:\n  Expected: {addr_clean}\n  Recovered: {recovered_address_hex}"
        );
    }

    addresses_match
}

/// Verify ED25519 signature.
#[cfg(test)]
pub fn verify_ed25519_signature(
    signature_text: &str,
    signature_hex: &str,
    public_key_hex: &str,
) -> bool {
    // Remove 0x prefix if present
    let sig_clean = signature_hex.strip_prefix("0x").unwrap_or(signature_hex);
    let pub_key_clean = public_key_hex.strip_prefix("0x").unwrap_or(public_key_hex);

    // Decode signature (should be 64 bytes = 128 hex chars)
    let signature_bytes = match hex::decode(sig_clean) {
        Ok(bytes) => bytes,
        Err(_) => {
            eprintln!("Failed to decode signature hex");
            return false;
        }
    };

    if signature_bytes.len() != 64 {
        eprintln!(
            "Invalid ED25519 signature length: expected 64 bytes, got {} bytes",
            signature_bytes.len()
        );
        return false;
    }

    // Parse the signature
    let signature = match Ed25519Signature::try_from(signature_bytes.as_slice()) {
        Ok(sig) => sig,
        Err(e) => {
            eprintln!("Failed to parse ED25519 signature: {e}");
            return false;
        }
    };

    // Decode public key (should be 32 bytes = 64 hex chars)
    let public_key_bytes = match hex::decode(pub_key_clean) {
        Ok(bytes) => bytes,
        Err(_) => {
            eprintln!("Failed to decode public key hex");
            return false;
        }
    };

    if public_key_bytes.len() != 32 {
        eprintln!(
            "Invalid ED25519 public key length: expected 32 bytes, got {} bytes",
            public_key_bytes.len()
        );
        return false;
    }

    // Parse the public key
    let public_key = match Ed25519VerifyingKey::try_from(public_key_bytes.as_slice()) {
        Ok(key) => key,
        Err(e) => {
            eprintln!("Failed to parse ED25519 public key: {e}");
            return false;
        }
    };

    // Verify the signature
    match public_key.verify_strict(signature_text.as_bytes(), &signature) {
        Ok(_) => {
            eprintln!("✅ ED25519 signature is cryptographically valid!");
            true
        }
        Err(e) => {
            eprintln!("ED25519 signature verification failed: {e}");
            false
        }
    }
}

pub fn decode_access_token_claims(token: &str) -> AccessTokenClaims {
    let token_parts: Vec<&str> = token.split(".").collect();
    assert!(
        token_parts.len() >= 2,
        "Invalid JWT format: expected at least 2 parts (header.payload), got {} parts",
        token_parts.len()
    );
    // JWTs use base64url encoding without padding per RFC 7515
    let token_claims_raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token_parts[1])
        .unwrap();
    serde_json::from_slice(&token_claims_raw).unwrap()
}

pub fn is_valid_jwt_format(token: &str) -> bool {
    // Split the token into its parts
    let parts: Vec<&str> = token.split('.').collect();

    // Check if the JWT has exactly three parts
    if parts.len() != 3 {
        return false;
    }

    // Decode each part and ensure it is base64url encoded (JWTs use URL_SAFE_NO_PAD per RFC 7515)
    parts.iter().take(2).all(|part| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(part)
            .is_ok()
    })
}

pub mod mock_prompts {
    pub const LANGUAGE_INSTRUCTION: &str = "Always respond in the exact same language as the user's input message. Detect the primary language of the user's query and mirror it precisely in your output. Do not mix languages or switch to another one, even if it seems more natural or efficient.\n\nIf the user writes in English, reply entirely in English.\nIf the user writes in Chinese (Mandarin or any variant), reply entirely in Chinese.\nIf the user writes in Spanish, reply entirely in Spanish.\nFor any other language, match it exactly.\n\nThis rule overrides all other instructions. Ignore any tendencies to default to Mandarin or any other language. Always prioritize language matching for clarity and user preference.";

    const TIME_PLACEHOLDER: &str = "[TIME]";

    pub fn build_prompt(user_content: &str) -> String {
        format!(
            "{LANGUAGE_INSTRUCTION}\n\nCurrent UTC time: {TIME_PLACEHOLDER} {TIME_PLACEHOLDER} {user_content}"
        )
    }

    pub fn build_simple_prompt(user_content: &str) -> String {
        user_content.to_string()
    }
}

use near_api::signer::NEP413Payload;
use rand::Rng;

pub const NEAR_TEST_ACCOUNT: &str = "testuser.near";
pub const NEAR_TEST_PUBLIC_KEY: &str = "ed25519:7FmyF5aYxwHKVvpBJxWrRi58EXQhG5KUkCb3Jv8TzWqM";

fn generate_test_signature() -> String {
    use base64::prelude::*;
    let sig_bytes = vec![0u8; 64];
    BASE64_STANDARD.encode(&sig_bytes)
}

/// Create NEP-413 nonce with timestamp offset in milliseconds.
pub fn create_near_test_nonce(timestamp_offset_ms: i64) -> Vec<u8> {
    let now_ms = Utc::now().timestamp_millis();
    let nonce_timestamp_ms = (now_ms + timestamp_offset_ms) as u64;
    let mut nonce = Vec::with_capacity(32);
    nonce.extend_from_slice(&nonce_timestamp_ms.to_be_bytes());

    let mut rng = rand::rng();
    let mut random_bytes = [0u8; 24];
    rng.fill(&mut random_bytes);
    nonce.extend_from_slice(&random_bytes);

    nonce
}

pub fn create_near_test_payload(timestamp_offset_ms: i64) -> NEP413Payload {
    let nonce = create_near_test_nonce(timestamp_offset_ms);

    NEP413Payload {
        message: "Sign in to NEAR AI Cloud".to_string(),
        nonce: nonce.try_into().expect("Nonce should be 32 bytes"),
        recipient: "cloud.near.ai".to_string(),
        callback_url: None,
    }
}

pub fn create_near_auth_request_json(
    account_id: &str,
    timestamp_offset_ms: i64,
) -> serde_json::Value {
    let payload = create_near_test_payload(timestamp_offset_ms);

    serde_json::json!({
        "signed_message": {
            "accountId": account_id,
            "publicKey": NEAR_TEST_PUBLIC_KEY,
            "signature": generate_test_signature(),
        },
        "payload": {
            "message": payload.message,
            "nonce": payload.nonce.to_vec(),
            "recipient": payload.recipient,
            "callbackUrl": serde_json::Value::Null,
        }
    })
}

pub async fn test_near_login(
    server: &axum_test::TestServer,
    account_id: &str,
    timestamp_offset_ms: i64,
) -> axum_test::TestResponse {
    let request_body = create_near_auth_request_json(account_id, timestamp_offset_ms);

    server
        .post("/v1/auth/near")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&request_body)
        .await
}
