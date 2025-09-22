//! End-to-end integration tests for the API
//!
//! These tests demonstrate how to use the exported functions from lib.rs
//! to set up and test the application.
//!
//! To run these tests, you need a PostgreSQL database running:
//! ```bash
//! # Using Docker:
//! docker run --name test-postgres -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=platform_api_test -p 5432:5432 -d postgres:15
//!
//! # Then run tests:
//! TEST_DATABASE_URL=postgresql://postgres:postgres@localhost:5432/platform_api_test cargo test --package api --test e2e_test
//! ```

use api::{
    build_app, init_auth_services, init_database_with_config, init_domain_services,
    models::{ConversationContentPart, ConversationItem},
};
use config::ApiConfig;

/// Helper function to create a test configuration
fn test_config() -> ApiConfig {
    ApiConfig {
        providers: vec![config::ProviderConfig {
            name: "vllm-prod-1".to_string(),
            provider_type: "vllm".to_string(),
            url: "http://REDACTED_IP2:8000".to_string(),
            api_key: Some("REDACTED".to_string()),
            enabled: true,
            priority: 1,
        }],
        server: config::ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0, // Use port 0 to get a random available port
        },
        model_discovery: config::ModelDiscoveryConfig {
            refresh_interval: 0,
            timeout: 5,
        },
        logging: config::LoggingConfig {
            level: "debug".to_string(), // Keep logs quiet during tests
            format: "compact".to_string(),
            modules: std::collections::HashMap::new(),
        },
        dstack_client: config::DstackClientConfig {
            url: "http://localhost:8000".to_string(),
        },
        auth: config::AuthConfig {
            enabled: true,
            github: None,
            google: None,
        },
        database: db_config_for_tests(),
    }
}

/// Helper function to create test database configuration
fn db_config_for_tests() -> config::DatabaseConfig {
    // Default test database config
    config::DatabaseConfig {
        host: std::env::var("TEST_DB_HOST").unwrap_or_else(|_| "localhost".to_string()),
        port: std::env::var("TEST_DB_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(5432),
        database: std::env::var("TEST_DB_NAME").unwrap_or_else(|_| "platform_api".to_string()),
        username: std::env::var("TEST_DB_USER").unwrap_or_else(|_| "postgres".to_string()),
        password: std::env::var("TEST_DB_PASSWORD").unwrap_or_else(|_| "postgres".to_string()),
        max_connections: 5,
    }
}

fn get_session_id() -> String {
    "402af343-70ba-4a8a-b926-012f71e86769".to_string()
}

#[tokio::test]
async fn test_models_api() {
    // Setup
    let config = test_config();
    let database = init_database_with_config(&config.database).await;
    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(
        database,
        auth_components,
        domain_services,
        config.auth.enabled,
    );

    let server = axum_test::TestServer::new(app).unwrap();
    let response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 200);
    let models = response.json::<api::models::ModelsResponse>();
    assert!(!models.data.is_empty());
}

#[tokio::test]
async fn test_responses_api() {
    let config = test_config();
    let database = init_database_with_config(&config.database).await;
    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(
        database,
        auth_components,
        domain_services,
        config.auth.enabled,
    );

    let server = axum_test::TestServer::new(app).unwrap();

    let response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    let models = response.json::<api::models::ModelsResponse>();
    assert!(!models.data.is_empty());

    let conversation = create_conversation(&server).await;
    println!("Conversation: {:?}", conversation);

    let message = "Hello".to_string();
    let response = create_response(
        &server,
        conversation.id.clone(),
        models.data[0].id.clone(),
        message.clone(),
    )
    .await;
    println!("Response: {:?}", response);

    let conversation_items = list_conversation_items(&server, conversation.id).await;
    assert_eq!(conversation_items.data.len(), 2);
    match &conversation_items.data[0] {
        ConversationItem::Message { content, .. } => {
            if let ConversationContentPart::InputText { text } = &content[0] {
                assert_eq!(text, message.as_str());
            }
        }
    }
}

async fn create_conversation(server: &axum_test::TestServer) -> api::models::ConversationObject {
    let response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!({
            "name": "Test Conversation",
            "description": "A test conversation"
        }))
        .await;
    assert_eq!(response.status_code(), 201);
    response.json::<api::models::ConversationObject>()
}

async fn get_conversation(
    server: &axum_test::TestServer,
    conversation_id: String,
) -> api::models::ConversationObject {
    let response = server
        .get(format!("/v1/conversations/{conversation_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::ConversationObject>()
}

async fn list_conversation_items(
    server: &axum_test::TestServer,
    conversation_id: String,
) -> api::models::ConversationItemList {
    let response = server
        .get(format!("/v1/conversations/{conversation_id}/items").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::ConversationItemList>()
}

async fn create_response(
    server: &axum_test::TestServer,
    conversation_id: String,
    model: String,
    message: String,
) -> api::models::ResponseObject {
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation_id,
            },
            "input": message,
            "temperature": 0.5,
            "max_output_tokens": 10,
            "stream": false,
            "model": model
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::ResponseObject>()
}

#[tokio::test]
async fn test_conversations_api() {
    // Setup
    let config = test_config();
    let database = init_database_with_config(&config.database).await;
    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(
        database,
        auth_components,
        domain_services,
        config.auth.enabled,
    );

    let server = axum_test::TestServer::new(app).unwrap();

    // Test that we can list conversations (should return empty array initially)
    let response = server
        .get("/v1/conversations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;
    assert_eq!(response.status_code(), 200);

    // Test creating a conversation
    let create_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!({
            "name": "Test Conversation",
            "description": "A test conversation"
        }))
        .await;
    assert_eq!(create_response.status_code(), 201);
}
