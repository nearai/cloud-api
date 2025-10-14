#![allow(dead_code)]

use api::{
    build_app, init_auth_services, init_domain_services, models::BatchUpdateModelApiRequest,
};
use chrono::Utc;
use config::ApiConfig;
use database::Database;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::OnceCell;

// Global once cell to ensure migrations only run once across all tests
static MIGRATIONS_INITIALIZED: OnceCell<()> = OnceCell::const_new();

// Constants for mock test data
pub const MOCK_USER_ID: &str = "11111111-1111-1111-1111-111111111111";

/// Helper function to create a test configuration
pub fn test_config() -> ApiConfig {
    ApiConfig {
        server: config::ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0, // Use port 0 to get a random available port
        },
        model_discovery: config::ModelDiscoveryConfig {
            discovery_server_url: "http://localhost:8080/models".to_string(),
            api_key: Some("REDACTED".to_string()),
            refresh_interval: 3600, // 1 hour - large value to avoid refresh during tests
            timeout: 5,
        },
        logging: config::LoggingConfig {
            level: "debug".to_string(),
            format: "compact".to_string(),
            modules: std::collections::HashMap::new(),
        },
        dstack_client: config::DstackClientConfig {
            url: "http://localhost:8000".to_string(),
        },
        auth: config::AuthConfig {
            mock: true,
            github: None,
            google: None,
            admin_domains: vec!["test.com".to_string()],
        },
        database: db_config_for_tests(),
    }
}

/// Helper function to create test database configuration
fn db_config_for_tests() -> config::DatabaseConfig {
    // Load database config from config file for tests
    // Falls back to localhost defaults if config file is not available
    match config::ApiConfig::load() {
        Ok(mut config) => {
            // Override max_connections to prevent pool exhaustion when running tests in parallel
            // Each test creates its own connection pool, so we need to keep this small
            config.database.max_connections = 2;
            config.database
        }
        Err(_) => {
            // Fallback to localhost defaults (for running tests without config file)
            config::DatabaseConfig {
                host: "localhost".to_string(),
                port: 5432,
                database: "platform_api".to_string(),
                username: "postgres".to_string(),
                password: "postgres".to_string(),
                max_connections: 2,
                tls_enabled: false,
                tls_ca_cert_path: None,
            }
        }
    }
}

pub fn get_session_id() -> String {
    "402af343-70ba-4a8a-b926-012f71e86769".to_string()
}

/// Initialize database with migrations running only once
pub async fn init_test_database(config: &config::DatabaseConfig) -> Arc<Database> {
    let database = Arc::new(
        Database::from_config(config)
            .await
            .expect("Failed to connect to database"),
    );

    // Ensure migrations only run once across all parallel tests
    MIGRATIONS_INITIALIZED
        .get_or_init(|| async {
            database
                .run_migrations()
                .await
                .expect("Failed to run database migrations");
        })
        .await;

    database
}

/// Setup a complete test server with all components initialized
/// Returns the test server ready for making requests
pub async fn setup_test_server() -> axum_test::TestServer {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::level_filters::LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock user in database for foreign key constraints
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(
        database.clone(),
        &config,
        auth_components.organization_service.clone(),
    )
    .await;

    let app = build_app(database, auth_components, domain_services);
    axum_test::TestServer::new(app).unwrap()
}

/// Create the mock user in the database to satisfy foreign key constraints
pub async fn assert_mock_user_in_db(database: &Arc<Database>) {
    let pool = database.pool();
    let client = pool.get().await.expect("Failed to get database connection");

    // Insert mock user if it doesn't exist with admin domain email
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

/// Create an organization
pub async fn create_org(server: &axum_test::TestServer) -> api::models::OrganizationResponse {
    let request = api::models::CreateOrganizationRequest {
        name: uuid::Uuid::new_v4().to_string(),
        description: Some("A test organization".to_string()),
        display_name: Some("Test Organization".to_string()),
    };
    let response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!(request))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::OrganizationResponse>()
}

/// Create an organization and set spending limit
/// Returns the organization response
pub async fn setup_org_with_credits(
    server: &axum_test::TestServer,
    amount_nano_dollars: i64,
) -> api::models::OrganizationResponse {
    let org = create_org(server).await;

    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": amount_nano_dollars,
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Test credits"
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    assert_eq!(response.status_code(), 200, "Failed to set credits");
    org
}

/// List workspaces for an organization
pub async fn list_workspaces(
    server: &axum_test::TestServer,
    org_id: String,
) -> Vec<api::routes::workspaces::WorkspaceResponse> {
    let response = server
        .get(format!("/v1/organizations/{}/workspaces", org_id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;
    assert_eq!(response.status_code(), 200);
    let list_response = response.json::<api::routes::workspaces::ListWorkspacesResponse>();
    list_response.workspaces
}

/// Create an API key in a workspace
pub async fn create_api_key_in_workspace(
    server: &axum_test::TestServer,
    workspace_id: String,
    name: String,
) -> api::models::ApiKeyResponse {
    let request = api::models::CreateApiKeyRequest {
        name,
        expires_at: Some(Utc::now() + chrono::Duration::days(90)),
        spend_limit: None,
    };
    let response = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace_id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!(request))
        .await;
    assert_eq!(response.status_code(), 201);
    response.json::<api::models::ApiKeyResponse>()
}

/// Get an API key for an organization (using its default workspace)
/// Returns the API key string
pub async fn get_api_key_for_org(server: &axum_test::TestServer, org_id: String) -> String {
    let workspaces = list_workspaces(server, org_id).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp =
        create_api_key_in_workspace(server, workspace.id.clone(), "Test API Key".to_string()).await;
    api_key_resp.key.unwrap()
}

/// Create an organization and API key
/// Returns the API key string and the API key response
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

/// Setup a test model with pricing
/// Returns the model name
pub async fn setup_test_model(server: &axum_test::TestServer) -> String {
    let batch = generate_model();
    let model_name = batch.keys().next().unwrap().clone();
    admin_batch_upsert_models(server, batch, get_session_id()).await;
    model_name
}

/// Generate a test model with standard pricing
pub fn generate_model() -> BatchUpdateModelApiRequest {
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
        serde_json::from_value(serde_json::json!({
            "publicName": "qwen/qwen3-30b-instruct",
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
    batch
}

/// Admin batch upsert models
pub async fn admin_batch_upsert_models(
    server: &axum_test::TestServer,
    models: BatchUpdateModelApiRequest,
    session_id: String,
) -> Vec<api::models::ModelWithPricing> {
    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", session_id))
        .json(&models)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Admin batch upsert should succeed"
    );
    response.json::<Vec<api::models::ModelWithPricing>>()
}

/// List models using an API key
pub async fn list_models(
    server: &axum_test::TestServer,
    api_key: String,
) -> api::models::ModelsResponse {
    let response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::ModelsResponse>()
}

/// Compute SHA256 hash of a string
pub fn compute_sha256(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    format!("{:x}", hasher.finalize())
}
