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
    models::{
        ConversationContentPart, ConversationItem, ResponseOutputContent, ResponseOutputItem,
    },
};
use config::ApiConfig;
use database::Database;
use std::sync::Arc;
use tracing::level_filters::LevelFilter;

// Constants for mock test data
const MOCK_USER_ID: &str = "11111111-1111-1111-1111-111111111111";

/// Helper function to create a test configuration
fn test_config() -> ApiConfig {
    ApiConfig {
        providers: vec![config::ProviderConfig {
            name: "vllm-prod-1".to_string(),
            provider_type: "vllm".to_string(),
            url: "http://160.72.54.186:8000".to_string(),
            api_key: Some("secret123".to_string()),
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
            mock: true,
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

/// Create the mock user in the database to satisfy foreign key constraints
async fn assert_mock_user_in_db(database: &Arc<Database>) {
    let pool = database.pool();
    let client = pool.get().await.expect("Failed to get database connection");

    // Insert mock user if it doesn't exist
    let _ = client.execute(
        "INSERT INTO users (id, email, username, display_name, avatar_url, auth_provider, provider_user_id, created_at, updated_at) 
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW())
         ON CONFLICT (id) DO NOTHING",
        &[
            &uuid::Uuid::parse_str(MOCK_USER_ID).unwrap(),
            &"test@example.com",
            &"testuser", 
            &Some("Test User".to_string()),
            &Some("https://example.com/avatar.jpg".to_string()),
            &"mock",
            &"mock_123",
        ],
    ).await.expect("Failed to create mock user");

    tracing::debug!("Mock user created/exists in database: {}", MOCK_USER_ID);
}

#[tokio::test]
async fn test_models_api() {
    // Setup
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();
    let config = test_config();
    let database = init_database_with_config(&config.database).await;

    // Create mock user in database for foreign key constraints
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database, auth_components, domain_services);

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
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_database_with_config(&config.database).await;

    // Create mock user in database for foreign key constraints
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database, auth_components, domain_services);

    let server = axum_test::TestServer::new(app).unwrap();

    let response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    let models = response.json::<api::models::ModelsResponse>();
    assert!(!models.data.is_empty());

    let conversation = create_conversation(&server).await;
    println!("Conversation: {:?}", conversation);

    let message = "Hello, how are you?".to_string();
    let max_tokens = 10;
    let response = create_response(
        &server,
        conversation.id.clone(),
        models.data[0].id.clone(),
        message.clone(),
        max_tokens,
    )
    .await;
    println!("Response: {:?}", response);
    assert!(response.output.iter().any(|o| {
        if let ResponseOutputItem::Message { content, .. } = o {
            content.iter().any(|c| {
                if let ResponseOutputContent::OutputText { text, .. } = c {
                    println!("Text: {}", text);
                    text.len() > max_tokens as usize
                } else {
                    false
                }
            })
        } else {
            false
        }
    }));

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

#[allow(dead_code)]
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
    max_tokens: u32,
) -> api::models::ResponseObject {
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation_id,
            },
            "input": message,
            "temperature": 0.7,
            "max_output_tokens": max_tokens,
            "stream": false,
            "model": model
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::ResponseObject>()
}

async fn create_response_stream(
    server: &axum_test::TestServer,
    conversation_id: String,
    model: String,
    message: String,
    max_tokens: u32,
) -> (String, api::models::ResponseObject) {
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation_id,
            },
            "input": message,
            "temperature": 0.7,
            "max_output_tokens": max_tokens,
            "stream": true,
            "model": model
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // For streaming responses, we get SSE events as text
    let response_text = response.text();

    let mut content = String::new();
    let mut final_response: Option<api::models::ResponseObject> = None;

    // Parse SSE format: "event: <type>\ndata: <json>\n\n"
    for line_chunk in response_text.split("\n\n") {
        if line_chunk.trim().is_empty() {
            continue;
        }

        let mut event_type = "";
        let mut event_data = "";

        for line in line_chunk.lines() {
            if let Some(event_name) = line.strip_prefix("event: ") {
                event_type = event_name;
            } else if let Some(data) = line.strip_prefix("data: ") {
                event_data = data;
            }
        }

        if !event_data.is_empty() {
            if let Ok(event_json) = serde_json::from_str::<serde_json::Value>(event_data) {
                match event_type {
                    "response.output_text.delta" => {
                        // Accumulate content deltas as they arrive
                        if let Some(delta) = event_json.get("delta").and_then(|v| v.as_str()) {
                            content.push_str(delta);
                            println!("Delta: {}", delta);
                        }
                    }
                    "response.completed" => {
                        // Extract final response from completed event
                        if let Some(response_obj) = event_json.get("response") {
                            final_response = Some(
                                serde_json::from_value::<api::models::ResponseObject>(
                                    response_obj.clone(),
                                )
                                .expect("Failed to parse response.completed event"),
                            );
                            println!("Stream completed");
                        }
                    }
                    "response.created" => {
                        println!("Response created");
                    }
                    "response.in_progress" => {
                        println!("Response in progress");
                    }
                    _ => {
                        println!("Event: {}", event_type);
                    }
                }
            }
        }
    }

    let final_resp =
        final_response.expect("Expected to receive response.completed event from stream");
    (content, final_resp)
}

#[tokio::test]
async fn test_conversations_api() {
    // Setup
    let config = test_config();
    let database = init_database_with_config(&config.database).await;

    // Create mock user in database for foreign key constraints
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database, auth_components, domain_services);

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

#[tokio::test]
async fn test_streaming_responses_api() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_database_with_config(&config.database).await;

    // Create mock user in database for foreign key constraints
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database, auth_components, domain_services);

    let server = axum_test::TestServer::new(app).unwrap();

    // Get available models
    let response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    let models = response.json::<api::models::ModelsResponse>();
    assert!(!models.data.is_empty());

    // Create a conversation
    let conversation = create_conversation(&server).await;
    println!("Conversation: {:?}", conversation);

    // Test streaming response
    let message = "Hello, how are you?".to_string();
    let (streamed_content, streaming_response) = create_response_stream(
        &server,
        conversation.id.clone(),
        models.data[0].id.clone(),
        message.clone(),
        50,
    )
    .await;

    println!("Streamed Content: {}", streamed_content);
    println!("Final Response: {:?}", streaming_response);

    // Verify we got content from the stream
    assert!(
        !streamed_content.is_empty(),
        "Expected non-empty streamed content"
    );

    // Verify the final response has content
    assert!(streaming_response.output.iter().any(|o| {
        if let ResponseOutputItem::Message { content, .. } = o {
            content.iter().any(|c| {
                if let ResponseOutputContent::OutputText { text, .. } = c {
                    println!("Final Response Text: {}", text);
                    !text.is_empty()
                } else {
                    false
                }
            })
        } else {
            false
        }
    }));

    // Verify streamed content matches final response content
    let final_text = streaming_response
        .output
        .iter()
        .filter_map(|o| {
            if let ResponseOutputItem::Message { content, .. } = o {
                content.iter().find_map(|c| {
                    if let ResponseOutputContent::OutputText { text, .. } = c {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        })
        .next()
        .unwrap_or_default();

    assert_eq!(
        streamed_content, final_text,
        "Streamed content should match final response text"
    );
}
