use api::{
    build_app, init_auth_services, init_database_with_config, init_domain_services,
    models::{
        ConversationContentPart, ConversationItem, ResponseOutputContent, ResponseOutputItem,
    },
};
use chrono::Utc;
use config::ApiConfig;
use database::Database;
use inference_providers::{models::ChatCompletionChunk, StreamChunk};
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
         ON CONFLICT DO NOTHING",
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

async fn create_org(server: &axum_test::TestServer) -> api::models::OrganizationResponse {
    let request = api::models::CreateOrganizationRequest {
        name: uuid::Uuid::new_v4().to_string(),
        description: Some("A test organization".to_string()),
        display_name: Some("Test Organization 2".to_string()),
    };
    let response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!(request))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::OrganizationResponse>()
}

async fn create_workspace(
    server: &axum_test::TestServer,
) -> api::routes::workspaces::WorkspaceResponse {
    let request = api::routes::workspaces::CreateWorkspaceRequest {
        name: uuid::Uuid::new_v4().to_string(),
        description: Some("A test workspace".to_string()),
        display_name: Some("Test Workspace".to_string()),
    };
    let response = server
        .post("/v1/workspaces")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!(request))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::routes::workspaces::WorkspaceResponse>()
}

async fn create_api_key_in_workspace(
    server: &axum_test::TestServer,
    workspace_id: String,
) -> api::models::ApiKeyResponse {
    let request = api::models::CreateApiKeyRequest {
        name: Some("Test API Key".to_string()),
        expires_at: Some(Utc::now() + chrono::Duration::days(90)),
    };
    let response = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace_id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!(request))
        .await;
    assert_eq!(response.status_code(), 201);
    response.json::<api::models::ApiKeyResponse>()
}

async fn list_workspaces(
    server: &axum_test::TestServer,
    org_id: String,
) -> Vec<api::routes::workspaces::WorkspaceResponse> {
    let response = server
        .get(format!("/v1/organizations/{}/workspaces", org_id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<Vec<api::routes::workspaces::WorkspaceResponse>>()
}

async fn create_org_and_api_key(
    server: &axum_test::TestServer,
) -> (String, api::models::ApiKeyResponse) {
    let org = create_org(server).await;
    println!("org: {:?}", org);

    let workspaces = list_workspaces(server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    println!("workspace: {:?}", workspace);
    // Fix: Use workspace.id instead of org.id
    let api_key_resp = create_api_key_in_workspace(server, workspace.id.clone()).await;
    println!("api_key_resp: {:?}", api_key_resp);
    (api_key_resp.key.clone().unwrap(), api_key_resp)
}

async fn list_models(
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

    let (api_key, _) = create_org_and_api_key(&server).await;
    let response = list_models(&server, api_key).await;

    assert!(!response.data.is_empty());
}

#[tokio::test]
async fn test_chat_completions_api() {
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

    let (api_key, _) = create_org_and_api_key(&server).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [
                {
                    "role": "user",
                    "content": "Hello, how are you?"
                }
            ],
            "stream": true,
            "max_tokens": 50
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // For streaming responses, we get SSE events as text
    let response_text = response.text();

    let mut content = String::new();
    let mut final_response: Option<ChatCompletionChunk> = None;

    // Parse standard OpenAI streaming format: "data: <json>"
    for line in response_text.lines() {
        println!("Line: {}", line);

        if let Some(data) = line.strip_prefix("data: ") {
            // Handle the [DONE] marker
            if data.trim() == "[DONE]" {
                println!("Stream completed with [DONE]");
                break;
            }

            // Parse JSON data
            if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                println!(
                    "Parsed JSON: {}",
                    serde_json::to_string_pretty(&chunk).unwrap_or_default()
                );

                let chat_chunk = match chunk {
                    StreamChunk::Chat(chat_chunk) => {
                        println!("Chat chunk: {:?}", chat_chunk);
                        Some(chat_chunk)
                    }
                    _ => {
                        println!("Unknown chunk: {:?}", chunk);
                        None
                    }
                }
                .unwrap();

                // Extract content from choices[0].delta.content
                if let Some(choice) = chat_chunk.choices.first() {
                    if let Some(delta) = &choice.delta {
                        if let Some(delta_content) = &delta.content {
                            content.push_str(delta_content.as_str());
                            println!("Delta content: '{}'", delta_content);
                        }

                        // Check if this is the final chunk (has usage or finish_reason)
                        if choice.finish_reason.is_some() || chat_chunk.usage.is_some() {
                            final_response = Some(chat_chunk.clone());
                            println!("Final chunk detected");
                        }
                    }
                }
            } else {
                println!("Failed to parse JSON: {}", data);
            }
        }
    }

    // Verify we got content from the stream
    assert!(!content.is_empty(), "Expected non-empty streamed content");

    println!("Streamed Content: {}", content);

    // Verify we got a meaningful response
    assert!(
        content.len() > 10,
        "Expected substantial content from stream, got: '{}'",
        content
    );

    // If we have a final response, verify its structure
    if let Some(final_resp) = final_response {
        println!("Final Response: {:?}", final_resp);
        assert!(
            !final_resp.choices.is_empty(),
            "Final response should have choices"
        );
        if let Some(choice) = final_resp.choices.first() {
            assert!(
                choice.delta.is_some(),
                "Final response choices should not be empty"
            );
        }
    } else {
        println!("No final response detected - this is okay for some streaming implementations");
    }
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
    let (api_key, _) = create_org_and_api_key(&server).await;

    let response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .await;

    let models = response.json::<api::models::ModelsResponse>();
    assert!(!models.data.is_empty());

    let conversation = create_conversation(&server, api_key.clone()).await;
    println!("Conversation: {:?}", conversation);

    let message = "Hello, how are you?".to_string();
    let max_tokens = 10;
    let response = create_response(
        &server,
        conversation.id.clone(),
        models.data[0].id.clone(),
        message.clone(),
        max_tokens,
        api_key.clone(),
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

    let conversation_items =
        list_conversation_items(&server, conversation.id, api_key.clone()).await;
    assert_eq!(conversation_items.data.len(), 2);
    match &conversation_items.data[0] {
        ConversationItem::Message { content, .. } => {
            if let ConversationContentPart::InputText { text } = &content[0] {
                assert_eq!(text, message.as_str());
            }
        }
    }
}

async fn create_conversation(
    server: &axum_test::TestServer,
    api_key: String,
) -> api::models::ConversationObject {
    let response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {}", api_key))
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
    api_key: String,
) -> api::models::ConversationObject {
    let response = server
        .get(format!("/v1/conversations/{conversation_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", api_key))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::ConversationObject>()
}

async fn list_conversation_items(
    server: &axum_test::TestServer,
    conversation_id: String,
    api_key: String,
) -> api::models::ConversationItemList {
    let response = server
        .get(format!("/v1/conversations/{conversation_id}/items").as_str())
        .add_header("Authorization", format!("Bearer {}", api_key))
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
    api_key: String,
) -> api::models::ResponseObject {
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {}", api_key))
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
    api_key: String,
) -> (String, api::models::ResponseObject) {
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {}", api_key))
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

    let (api_key, _) = create_org_and_api_key(&server).await;

    // Test that we can list conversations (should return empty array initially)
    let response = server
        .get("/v1/conversations")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .await;
    assert_eq!(response.status_code(), 200);

    // Test creating a conversation
    let create_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {}", api_key))
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
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Get available models
    let response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .await;

    let models = response.json::<api::models::ModelsResponse>();
    assert!(!models.data.is_empty());

    // Create a conversation
    let conversation = create_conversation(&server, api_key.clone()).await;
    println!("Conversation: {:?}", conversation);

    // Test streaming response
    let message = "Hello, how are you?".to_string();
    let (streamed_content, streaming_response) = create_response_stream(
        &server,
        conversation.id.clone(),
        models.data[0].id.clone(),
        message.clone(),
        50,
        api_key.clone(),
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
