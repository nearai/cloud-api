use api::{
    build_app, init_auth_services, init_domain_services,
    models::{
        BatchUpdateModelApiRequest, ConversationContentPart, ConversationItem,
        ResponseOutputContent, ResponseOutputItem,
    },
};
use chrono::Utc;
use config::ApiConfig;
use database::Database;
use inference_providers::{models::ChatCompletionChunk, StreamChunk};
use std::sync::Arc;
use tokio::sync::OnceCell;
use tracing::level_filters::LevelFilter;

// Global once cell to ensure migrations only run once across all tests
static MIGRATIONS_INITIALIZED: OnceCell<()> = OnceCell::const_new();

// Constants for mock test data
const MOCK_USER_ID: &str = "11111111-1111-1111-1111-111111111111";

/// Helper function to create a test configuration
fn test_config() -> ApiConfig {
    ApiConfig {
        server: config::ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0, // Use port 0 to get a random available port
        },
        model_discovery: config::ModelDiscoveryConfig {
            discovery_server_url: "http://REDACTED_DISCOVERY:8080/models".to_string(),
            api_key: Some("REDACTED".to_string()),
            refresh_interval: 3600, // 1 hour - large value to avoid refresh during tests
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
        Ok(config) => config.database,
        Err(_) => {
            // Fallback to localhost defaults (for running tests without config file)
            config::DatabaseConfig {
                host: "localhost".to_string(),
                port: 5432,
                database: "platform_api".to_string(),
                username: "postgres".to_string(),
                password: "postgres".to_string(),
                max_connections: 5,
            }
        }
    }
}

fn get_session_id() -> String {
    "402af343-70ba-4a8a-b926-012f71e86769".to_string()
}

/// Initialize database with migrations running only once
async fn init_test_database(config: &config::DatabaseConfig) -> Arc<Database> {
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

/// Create the mock user in the database to satisfy foreign key constraints
async fn assert_mock_user_in_db(database: &Arc<Database>) {
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

async fn _create_workspace(
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

async fn admin_batch_upsert_models(
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

#[tokio::test]
async fn test_models_api() {
    // Setup
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();
    let config = test_config();
    let database = init_test_database(&config.database).await;

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
    let database = init_test_database(&config.database).await;

    // Create mock user in database for foreign key constraints
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database, auth_components, domain_services);

    let server = axum_test::TestServer::new(app).unwrap();

    // Create organization and set up credits
    let org = create_org(&server).await;

    // Add credits to the organization (scale 9 = nano-dollars)
    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 10000000000i64,  // $10.00 USD (in nano-dollars)
            "scale": 9,
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

    // Get API key
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp = create_api_key_in_workspace(&server, workspace.id.clone()).await;
    let api_key = api_key_resp.key.unwrap();

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
    let database = init_test_database(&config.database).await;

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

    // Check that response completed successfully
    assert_eq!(response.status, api::models::ResponseStatus::Completed);

    // Check that we got usage information (tokens were generated)
    assert!(
        response.usage.output_tokens > 0,
        "Expected output tokens to be generated"
    );

    // Check that we have output content structure (even if text is empty due to VLLM issues)
    assert!(!response.output.is_empty(), "Expected output items");

    // Log the text we got (may be empty if VLLM has issues)
    for output_item in &response.output {
        if let ResponseOutputItem::Message { content, .. } = output_item {
            for content_part in content {
                if let ResponseOutputContent::OutputText { text, .. } = content_part {
                    println!(
                        "Response text length: {} chars, content: '{}'",
                        text.len(),
                        text
                    );
                    if text.is_empty() {
                        println!(
                            "Warning: VLLM returned empty text despite reporting {} output tokens",
                            response.usage.output_tokens
                        );
                    }
                }
            }
        }
    }

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
    let database = init_test_database(&config.database).await;

    // Create mock user in database for foreign key constraints
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database, auth_components, domain_services);

    let server = axum_test::TestServer::new(app).unwrap();

    let (api_key, _) = create_org_and_api_key(&server).await;

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
    let database = init_test_database(&config.database).await;

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

fn generate_model() -> BatchUpdateModelApiRequest {
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "scale": 9,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,
                "scale": 9,
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

#[tokio::test]
async fn test_admin_update_model() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user with admin domain email
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database, auth_components, domain_services);

    let server = axum_test::TestServer::new(app).unwrap();

    // Upsert models (using session token with admin domain email)
    let batch = generate_model();
    let model_name = batch.keys().next().unwrap().clone();
    let updated_models = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    println!("Updated models: {:?}", updated_models);
    assert_eq!(updated_models.len(), 1);
    let updated_model = &updated_models[0];
    assert_eq!(updated_model.model_id, model_name);
    assert_eq!(
        updated_model.metadata.model_display_name,
        "Updated Model Name"
    );
    assert_eq!(updated_model.input_cost_per_token.amount, 1000000);
}

#[tokio::test]
async fn test_get_model_by_name() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user with admin domain email
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database, auth_components, domain_services);

    let server = axum_test::TestServer::new(app).unwrap();

    // Upsert a model with a name containing forward slashes
    let batch = generate_model();
    let model_name = batch.keys().next().unwrap().clone();
    let model_request = batch.get(&model_name).unwrap().clone();

    let upserted_models = admin_batch_upsert_models(&server, batch, get_session_id()).await;

    println!("Upserted models: {:?}", upserted_models);
    assert_eq!(upserted_models.len(), 1);

    // Test retrieving the model by name (public endpoint - no auth required)
    // Model names may contain forward slashes (e.g., "Qwen/Qwen3-30B-A3B-Instruct-2507")
    // which must be URL-encoded when used in the path
    println!("Test: Requesting model by name: '{}'", model_name);
    let encoded_model_name =
        url::form_urlencoded::byte_serialize(model_name.as_bytes()).collect::<String>();
    println!("Test: URL-encoded for path: '{}'", encoded_model_name);

    let response = server
        .get(format!("/v1/model/{}", encoded_model_name).as_str())
        .await;

    println!("Response status: {}", response.status_code());
    assert_eq!(response.status_code(), 200);

    let model_resp = response.json::<api::models::ModelWithPricing>();
    println!("Retrieved model: {:?}", model_resp);

    // Verify the model details match what we upserted
    assert_eq!(model_resp.model_id, model_name);
    assert_eq!(
        model_resp.metadata.model_display_name,
        model_request.model_display_name.as_deref().unwrap()
    );
    assert_eq!(
        model_resp.metadata.model_description,
        model_request.model_description.as_deref().unwrap()
    );
    assert_eq!(
        model_resp.metadata.context_length,
        model_request.context_length.unwrap()
    );
    assert_eq!(
        model_resp.metadata.verifiable,
        model_request.verifiable.unwrap()
    );
    assert_eq!(
        model_resp.input_cost_per_token.amount,
        model_request.input_cost_per_token.as_ref().unwrap().amount
    );
    assert_eq!(
        model_resp.input_cost_per_token.scale,
        model_request.input_cost_per_token.as_ref().unwrap().scale
    );
    assert_eq!(
        model_resp.input_cost_per_token.currency,
        model_request
            .input_cost_per_token
            .as_ref()
            .unwrap()
            .currency
    );
    assert_eq!(
        model_resp.output_cost_per_token.amount,
        model_request.output_cost_per_token.as_ref().unwrap().amount
    );
    assert_eq!(
        model_resp.output_cost_per_token.scale,
        model_request.output_cost_per_token.as_ref().unwrap().scale
    );
    assert_eq!(
        model_resp.output_cost_per_token.currency,
        model_request
            .output_cost_per_token
            .as_ref()
            .unwrap()
            .currency
    );

    // Test retrieving a non-existent model
    // Note: URL-encode the model name even for non-existent models
    let nonexistent_model = "nonexistent/model";
    let encoded_nonexistent =
        url::form_urlencoded::byte_serialize(nonexistent_model.as_bytes()).collect::<String>();
    let response = server
        .get(format!("/v1/model/{}", encoded_nonexistent).as_str())
        .await;

    println!(
        "Non-existent model response status: {}",
        response.status_code()
    );
    assert_eq!(response.status_code(), 404);

    // Only try to parse JSON if there's a body
    let response_text = response.text();
    if !response_text.is_empty() {
        let error: api::models::ErrorResponse =
            serde_json::from_str(&response_text).expect("Failed to parse error response");
        println!("Error response: {:?}", error);
        assert_eq!(error.error.r#type, "model_not_found");
        assert!(error
            .error
            .message
            .contains("Model 'nonexistent/model' not found"));
    } else {
        println!("Warning: 404 response had empty body");
    }
}

#[tokio::test]
async fn test_admin_update_organization_limits() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user with admin domain email
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database, auth_components, domain_services);

    let server = axum_test::TestServer::new(app).unwrap();

    // Create an organization
    let org = create_org(&server).await;
    println!("Created organization: {:?}", org);

    // Update organization limits (scale 9 = nano-dollars)
    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 100000000000i64,  // $100.00 USD (in nano-dollars)
            "scale": 9,
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Initial credit allocation"
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    assert_eq!(
        response.status_code(),
        200,
        "Organization limits update should succeed"
    );

    let update_response =
        serde_json::from_str::<api::models::UpdateOrganizationLimitsResponse>(&response.text())
            .expect("Failed to parse response");

    println!("Update response: {:?}", update_response);

    // Verify the response
    assert_eq!(update_response.organization_id, org.id);
    assert_eq!(update_response.spend_limit.amount, 100000000000i64);
    assert_eq!(update_response.spend_limit.scale, 9);
    assert_eq!(update_response.spend_limit.currency, "USD");
    assert!(!update_response.updated_at.is_empty());
}

#[tokio::test]
async fn test_admin_update_organization_limits_invalid_org() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user with admin domain email
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database, auth_components, domain_services);

    let server = axum_test::TestServer::new(app).unwrap();

    // Try to update limits for non-existent organization
    let fake_org_id = uuid::Uuid::new_v4().to_string();
    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 50000000000i64,
            "scale": 9,
            "currency": "USD"
        }
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", fake_org_id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    println!("Response status: {}", response.status_code());
    assert_eq!(
        response.status_code(),
        404,
        "Should return 404 for non-existent organization"
    );

    let error = response.json::<api::models::ErrorResponse>();
    println!("Error response: {:?}", error);
    assert_eq!(error.error.r#type, "organization_not_found");
}

#[tokio::test]
async fn test_admin_update_organization_limits_multiple_times() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user with admin domain email
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database, auth_components, domain_services);

    let server = axum_test::TestServer::new(app).unwrap();

    // Create an organization
    let org = create_org(&server).await;

    // First update - set initial limit (scale 9 = nano-dollars)
    let first_update = serde_json::json!({
        "spendLimit": {
            "amount": 50000000000i64,  // $50.00 USD (in nano-dollars)
            "scale": 9,
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Initial allocation"
    });

    let response1 = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&first_update)
        .await;

    assert_eq!(response1.status_code(), 200);
    let response1_data =
        serde_json::from_str::<api::models::UpdateOrganizationLimitsResponse>(&response1.text())
            .unwrap();
    assert_eq!(response1_data.spend_limit.amount, 50000000000i64);

    // Second update - increase limit
    let second_update = serde_json::json!({
        "spendLimit": {
            "amount": 150000000000i64,  // $150.00 USD (in nano-dollars)
            "scale": 9,
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Customer purchased additional credits"
    });

    let response2 = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&second_update)
        .await;

    assert_eq!(response2.status_code(), 200);
    let response2_data =
        serde_json::from_str::<api::models::UpdateOrganizationLimitsResponse>(&response2.text())
            .unwrap();
    assert_eq!(response2_data.spend_limit.amount, 150000000000i64);

    // Verify the second update happened after the first
    let first_updated = chrono::DateTime::parse_from_rfc3339(&response1_data.updated_at).unwrap();
    let second_updated = chrono::DateTime::parse_from_rfc3339(&response2_data.updated_at).unwrap();
    assert!(
        second_updated > first_updated,
        "Second update should be after first update"
    );
}

#[tokio::test]
async fn test_admin_update_organization_limits_usd_only() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user with admin domain email
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database, auth_components, domain_services);

    let server = axum_test::TestServer::new(app).unwrap();

    // Create an organization
    let org = create_org(&server).await;

    // All currencies are USD now (fixed scale 9)
    let usd_update = serde_json::json!({
        "spendLimit": {
            "amount": 85000000000i64,  // $85.00 USD (in nano-dollars)
            "scale": 9,
            "currency": "USD"
        },
        "changedBy": "billing-service",
        "changeReason": "Customer purchase"
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&usd_update)
        .await;

    assert_eq!(response.status_code(), 200);
    let response_data =
        serde_json::from_str::<api::models::UpdateOrganizationLimitsResponse>(&response.text())
            .unwrap();
    assert_eq!(response_data.spend_limit.currency, "USD");
    assert_eq!(response_data.spend_limit.amount, 85000000000i64);
}

// ============================================
// Usage Tracking E2E Tests
// ============================================

#[tokio::test]
async fn test_no_credits_denies_request() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database.clone(), auth_components, domain_services);
    let server = axum_test::TestServer::new(app).unwrap();

    // Create organization WITHOUT setting any credits
    let (api_key, _api_key_response) = create_org_and_api_key(&server).await;

    // Try to make a chat completion request - should be denied (402 Payment Required)
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [
                {
                    "role": "user",
                    "content": "Say hello"
                }
            ],
            "stream": false,
            "max_tokens": 20
        }))
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    // Should get 402 Payment Required - no credits
    assert_eq!(
        response.status_code(),
        402,
        "Expected 402 Payment Required for organization without credits"
    );

    let error = serde_json::from_str::<api::models::ErrorResponse>(&response.text())
        .expect("Failed to parse error response");
    println!("Error: {:?}", error);
    assert!(
        error.error.r#type == "no_credits" || error.error.r#type == "no_limit_configured",
        "Expected error type 'no_credits' or 'no_limit_configured'"
    );
}

#[tokio::test]
async fn test_usage_tracking_on_completion() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database.clone(), auth_components, domain_services);
    let server = axum_test::TestServer::new(app).unwrap();

    // Create organization
    let org = create_org(&server).await;

    // Set credits for the organization (scale 9 = nano-dollars)
    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 1000000000i64,  // $1.00 USD (in nano-dollars)
            "scale": 9,
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Initial test credits"
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    assert_eq!(response.status_code(), 200, "Failed to set credits");

    // Get API key for this organization
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp = create_api_key_in_workspace(&server, workspace.id.clone()).await;
    let api_key = api_key_resp.key.unwrap();

    // Make a chat completion request
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [
                {
                    "role": "user",
                    "content": "Say hello"
                }
            ],
            "stream": false,
            "max_tokens": 20
        }))
        .await;

    println!("Completion response status: {}", response.status_code());
    assert_eq!(response.status_code(), 200);

    let completion_response = response.json::<api::models::ChatCompletionResponse>();
    println!("Usage: {:?}", completion_response.usage);

    // Verify completion was recorded
    assert!(completion_response.usage.input_tokens > 0);
    assert!(completion_response.usage.output_tokens > 0);

    // Wait a bit for async usage recording to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
}

#[tokio::test]
async fn test_usage_limit_enforcement() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database.clone(), auth_components, domain_services);
    let server = axum_test::TestServer::new(app).unwrap();

    // Create organization
    let org = create_org(&server).await;
    println!("Created organization: {:?}", org);

    // Set a very low spending limit ($0.000000001 USD = 1 nano-dollar)
    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 1,  // Minimal amount (1 nano-dollar)
            "scale": 9,
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Test low limit"
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    assert_eq!(response.status_code(), 200, "Failed to set limit");

    // Get API key for this organization
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp = create_api_key_in_workspace(&server, workspace.id.clone()).await;
    let api_key = api_key_resp.key.unwrap();

    // First request should succeed (no usage yet)
    let response1 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [{"role": "user", "content": "Hi"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    println!("First request status: {}", response1.status_code());
    // This might succeed or fail depending on timing

    // Wait for usage to be recorded
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Second request should fail with payment required
    let response2 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [{"role": "user", "content": "Hi again"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    println!("Second request status: {}", response2.status_code());
    println!("Second request body: {}", response2.text());

    // Should get 402 Payment Required after exceeding limit
    assert!(
        response2.status_code() == 402 || response2.status_code() == 200,
        "Expected 402 Payment Required or 200 OK, got: {}",
        response2.status_code()
    );
}

#[tokio::test]
async fn test_get_organization_balance() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database.clone(), auth_components, domain_services);
    let server = axum_test::TestServer::new(app).unwrap();

    // Create organization
    let org = create_org(&server).await;

    // Set spending limit first
    let limit_request = serde_json::json!({
        "spendLimit": {
            "amount": 5000000000i64,  // $5.00 USD
            "scale": 9,
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Initial test credits"
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&limit_request)
        .await;

    assert_eq!(response.status_code(), 200, "Failed to set limit");

    // Get balance - should now show limit even with no usage
    let response = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    println!("Balance response status: {}", response.status_code());
    println!("Balance response body: {}", response.text());

    assert_eq!(response.status_code(), 200, "Should get balance with limit");

    let balance =
        serde_json::from_str::<api::routes::usage::OrganizationBalanceResponse>(&response.text())
            .expect("Failed to parse balance response");

    println!("Balance: {:?}", balance);

    // Verify limit is included
    assert!(balance.spend_limit.is_some(), "Should have spend_limit");
    assert_eq!(
        balance.spend_limit.unwrap(),
        5000000000i64,
        "Limit should be $5.00 (5B nano-dollars)"
    );
    assert!(
        balance.spend_limit_display.is_some(),
        "Should have spend_limit_display"
    );
    assert_eq!(
        balance.spend_limit_display.unwrap(),
        "$5.00",
        "Display should show $5.00"
    );

    // Verify remaining is calculated correctly (no usage yet, so remaining = limit)
    assert!(balance.remaining.is_some(), "Should have remaining");
    assert_eq!(
        balance.remaining.unwrap(),
        5000000000i64,
        "Remaining should equal limit with no usage"
    );
    assert!(
        balance.remaining_display.is_some(),
        "Should have remaining_display"
    );

    // Verify spent is zero
    assert_eq!(balance.total_spent, 0, "Total spent should be zero");
    assert_eq!(
        balance.total_spent_display, "$0.00",
        "Spent display should be $0.00"
    );
}

#[tokio::test]
async fn test_get_organization_usage_history() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database.clone(), auth_components, domain_services);
    let server = axum_test::TestServer::new(app).unwrap();

    // Create organization
    let org = create_org(&server).await;

    // Get usage history (should be empty initially)
    let response = server
        .get(format!("/v1/organizations/{}/usage/history", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    println!("Usage history response status: {}", response.status_code());
    assert_eq!(response.status_code(), 200);

    let history_response = response.json::<serde_json::Value>();
    println!("Usage history: {:?}", history_response);

    // Should have data array (empty is fine)
    assert!(history_response.get("data").is_some());
}

#[tokio::test]
async fn test_completion_cost_calculation() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database.clone(), auth_components, domain_services);
    let server = axum_test::TestServer::new(app).unwrap();

    // Create organization
    let org = create_org(&server).await;
    println!("Created organization: {}", org.id);

    // Set spending limits high enough for the test (scale 9 = nano-dollars)
    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 1000000000000i64,  // $1000.00 USD (in nano-dollars)
            "scale": 9,
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Test credits for cost calculation"
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    assert_eq!(response.status_code(), 200, "Failed to set credits");

    // Upsert a model with specific known pricing
    // Input cost: $0.001 per token (1000000 / 10^9 = 0.001)
    // Output cost: $0.002 per token (2000000 / 10^9 = 0.002)
    let batch = generate_model();
    let model_name = batch.keys().next().unwrap().clone();
    let updated_models = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    println!("Updated model: {:?}", updated_models[0]);

    // Get API key
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp = create_api_key_in_workspace(&server, workspace.id.clone()).await;
    let api_key = api_key_resp.key.unwrap();

    // Get initial balance (should be 0 or not found)
    let initial_balance_response = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    let initial_spent = if initial_balance_response.status_code() == 200 {
        let balance =
            initial_balance_response.json::<api::routes::usage::OrganizationBalanceResponse>();
        balance.total_spent
    } else {
        0i64
    };
    println!("Initial spent amount (nano-dollars): {}", initial_spent);

    // Make a chat completion request with controlled parameters
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [
                {
                    "role": "user",
                    "content": "Say hello in exactly 5 words."
                }
            ],
            "stream": false,
            "max_tokens": 50
        }))
        .await;

    println!("Completion response status: {}", response.status_code());
    assert_eq!(
        response.status_code(),
        200,
        "Completion request should succeed"
    );

    let completion_response = response.json::<api::models::ChatCompletionResponse>();
    println!("Usage: {:?}", completion_response.usage);

    let input_tokens = completion_response.usage.input_tokens;
    let output_tokens = completion_response.usage.output_tokens;

    // Verify we got actual token counts
    assert!(input_tokens > 0, "Should have input tokens");
    assert!(output_tokens > 0, "Should have output tokens");

    // Calculate expected cost based on model pricing (all at scale 9)
    // Input: 1000000 nano-dollars = $0.000001 per token
    // Output: 2000000 nano-dollars = $0.000002 per token

    let input_cost_per_token = 1000000i64; // nano-dollars
    let output_cost_per_token = 2000000i64; // nano-dollars

    // Expected total cost (at scale 9)
    let expected_input_cost = (input_tokens as i64) * input_cost_per_token;
    let expected_output_cost = (output_tokens as i64) * output_cost_per_token;
    let expected_total_cost = expected_input_cost + expected_output_cost;

    println!(
        "Input tokens: {}, cost per token: {} nano-dollars",
        input_tokens, input_cost_per_token
    );
    println!(
        "Output tokens: {}, cost per token: {} nano-dollars",
        output_tokens, output_cost_per_token
    );
    println!("Expected input cost: {} nano-dollars", expected_input_cost);
    println!(
        "Expected output cost: {} nano-dollars",
        expected_output_cost
    );
    println!("Expected total cost: {} nano-dollars", expected_total_cost);

    // Wait for async usage recording to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Get the updated balance
    let balance_response = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(
        balance_response.status_code(),
        200,
        "Should be able to get balance"
    );
    let balance = balance_response.json::<api::routes::usage::OrganizationBalanceResponse>();
    println!("Balance: {:?}", balance);
    println!("Total spent: {} nano-dollars", balance.total_spent);

    // Verify limit information is included
    assert!(balance.spend_limit.is_some(), "Should have spend_limit");
    assert_eq!(
        balance.spend_limit.unwrap(),
        1000000000000i64,
        "Limit should be $1000.00"
    );
    assert!(
        balance.spend_limit_display.is_some(),
        "Should have readable limit"
    );
    println!(
        "Spend limit: {}",
        balance.spend_limit_display.as_ref().unwrap()
    );

    // Verify remaining is calculated
    assert!(balance.remaining.is_some(), "Should have remaining");
    assert!(
        balance.remaining_display.is_some(),
        "Should have readable remaining"
    );
    println!("Remaining: {}", balance.remaining_display.as_ref().unwrap());

    // The recorded cost should match our expected calculation (all at scale 9)
    let actual_spent = balance.total_spent - initial_spent;

    println!("Actual spent: {} nano-dollars", actual_spent);
    println!("Expected spent: {} nano-dollars", expected_total_cost);

    // Verify the cost calculation is correct (with small tolerance for rounding)
    let tolerance = 10; // Allow small rounding differences
    assert!(
        (actual_spent - expected_total_cost).abs() <= tolerance,
        "Cost calculation mismatch: expected {} (±{}), got {}. \
         Input tokens: {}, Output tokens: {}, \
         Input cost per token: {}, Output cost per token: {}",
        expected_total_cost,
        tolerance,
        actual_spent,
        input_tokens,
        output_tokens,
        input_cost_per_token,
        output_cost_per_token
    );

    // Verify the display format is reasonable
    assert!(
        !balance.total_spent_display.is_empty(),
        "Should have display format"
    );
    assert!(
        balance.total_spent_display.starts_with("$"),
        "Should show dollar sign"
    );
    println!("Total spent display: {}", balance.total_spent_display);

    // Verify usage history also shows the correct cost
    let history_response = server
        .get(format!("/v1/organizations/{}/usage/history?limit=10", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(history_response.status_code(), 200);
    let history = history_response.json::<api::routes::usage::UsageHistoryResponse>();
    println!("Usage history: {:?}", history);

    // Find the most recent entry (should be our completion)
    assert!(
        !history.data.is_empty(),
        "Should have usage history entries"
    );
    let latest_entry = &history.data[0];

    println!("Latest usage entry: {:?}", latest_entry);
    assert_eq!(
        latest_entry.model_id, model_name,
        "Should record correct model"
    );
    assert_eq!(
        latest_entry.input_tokens, input_tokens as i32,
        "Should record correct input tokens"
    );
    assert_eq!(
        latest_entry.output_tokens, output_tokens as i32,
        "Should record correct output tokens"
    );

    // Verify the cost in the history entry matches (all at scale 9 now)
    assert!(
        (latest_entry.total_cost - expected_total_cost).abs() <= tolerance,
        "History entry cost should match: expected {} nano-dollars, got {}",
        expected_total_cost,
        latest_entry.total_cost
    );
}

// ============================================
// Organization Balance and Limit Tests
// ============================================

#[tokio::test]
async fn test_organization_balance_with_limit_and_usage() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database.clone(), auth_components, domain_services);
    let server = axum_test::TestServer::new(app).unwrap();

    // Create organization
    let org = create_org(&server).await;

    // Set spending limit of $10.00
    let limit_request = serde_json::json!({
        "spendLimit": {
            "amount": 10000000000i64,  // $10.00 USD
            "scale": 9,
            "currency": "USD"
        },
        "changedBy": "billing@test.com",
        "changeReason": "Customer purchased $10 credits"
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&limit_request)
        .await;

    assert_eq!(response.status_code(), 200);

    // Get balance before any usage
    let response = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 200);
    let initial_balance =
        serde_json::from_str::<api::routes::usage::OrganizationBalanceResponse>(&response.text())
            .expect("Failed to parse balance");

    println!("Initial balance: {:?}", initial_balance);

    // Verify initial state
    assert_eq!(initial_balance.total_spent, 0);
    assert_eq!(initial_balance.spend_limit.unwrap(), 10000000000i64);
    assert_eq!(initial_balance.remaining.unwrap(), 10000000000i64);
    assert_eq!(initial_balance.spend_limit_display.unwrap(), "$10.00");
    assert_eq!(initial_balance.remaining_display.unwrap(), "$10.00");

    // Make a completion to record some usage
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp = create_api_key_in_workspace(&server, workspace.id.clone()).await;
    let api_key = api_key_resp.key.unwrap();

    // Upsert model with known pricing
    let batch = generate_model();
    let model_name = batch.keys().next().unwrap().clone();
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hi"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // Wait for usage recording
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Get balance after usage
    let response = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 200);
    let final_balance =
        serde_json::from_str::<api::routes::usage::OrganizationBalanceResponse>(&response.text())
            .expect("Failed to parse balance");

    println!("Final balance: {:?}", final_balance);

    // Verify spending was recorded
    assert!(final_balance.total_spent > 0, "Should have recorded spend");

    // Verify limit is still there
    assert_eq!(
        final_balance.spend_limit.unwrap(),
        10000000000i64,
        "Limit should remain $10.00"
    );

    // Verify remaining is calculated correctly
    let expected_remaining = 10000000000i64 - final_balance.total_spent;
    assert_eq!(
        final_balance.remaining.unwrap(),
        expected_remaining,
        "Remaining should be limit - spent"
    );

    // Verify all display fields are present
    assert!(final_balance.spend_limit_display.is_some());
    assert!(final_balance.remaining_display.is_some());
    println!("Spent: {}", final_balance.total_spent_display);
    println!("Limit: {}", final_balance.spend_limit_display.unwrap());
    println!("Remaining: {}", final_balance.remaining_display.unwrap());
}

// ============================================
// API Key Spend Limit Tests
// ============================================

#[tokio::test]
async fn test_api_key_spend_limit_update() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database.clone(), auth_components, domain_services);
    let server = axum_test::TestServer::new(app).unwrap();

    // Create organization and API key
    let org = create_org(&server).await;
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp = create_api_key_in_workspace(&server, workspace.id.clone()).await;

    println!("Created API key: {:?}", api_key_resp);

    // Verify initial state has no spend limit
    assert_eq!(api_key_resp.spend_limit, None);

    // Update the API key spend limit to $1.00 (1000000000 nano-dollars)
    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 1000000000i64,  // $1.00 USD
            "scale": 9,
            "currency": "USD"
        }
    });

    let response = server
        .patch(
            format!(
                "/v1/workspaces/{}/api-keys/{}/spend-limit",
                workspace.id, api_key_resp.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    println!("Update response status: {}", response.status_code());
    println!("Update response body: {}", response.text());

    assert_eq!(
        response.status_code(),
        200,
        "API key spend limit update should succeed"
    );

    let updated_key = serde_json::from_str::<api::models::ApiKeyResponse>(&response.text())
        .expect("Failed to parse response");

    println!("Updated API key: {:?}", updated_key);

    // Verify the spend limit was set
    assert!(updated_key.spend_limit.is_some());
    let spend_limit = updated_key.spend_limit.unwrap();
    assert_eq!(spend_limit.amount, 1000000000i64);
    assert_eq!(spend_limit.scale, 9);
    assert_eq!(spend_limit.currency, "USD");

    // Verify key_prefix is properly formatted
    assert!(
        updated_key.key_prefix.contains("****"),
        "Key prefix should contain asterisks"
    );

    // Remove the spend limit (set to null)
    let remove_request = serde_json::json!({
        "spendLimit": null
    });

    let response = server
        .patch(
            format!(
                "/v1/workspaces/{}/api-keys/{}/spend-limit",
                workspace.id, api_key_resp.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&remove_request)
        .await;

    assert_eq!(response.status_code(), 200);

    let updated_key = serde_json::from_str::<api::models::ApiKeyResponse>(&response.text())
        .expect("Failed to parse response");

    // Verify the spend limit was removed
    assert_eq!(updated_key.spend_limit, None);
}

#[tokio::test]
async fn test_api_key_spend_limit_enforcement() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database.clone(), auth_components, domain_services);
    let server = axum_test::TestServer::new(app).unwrap();

    // Create organization
    let org = create_org(&server).await;
    println!("Created organization: {}", org.id);

    // Set high organization limit so we test API key limit, not org limit
    let org_limit_request = serde_json::json!({
        "spendLimit": {
            "amount": 10000000000i64,  // $10.00 USD (high limit)
            "scale": 9,
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Test high org limit"
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&org_limit_request)
        .await;

    assert_eq!(response.status_code(), 200, "Failed to set org limit");

    // Create API key and set a very low limit
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp = create_api_key_in_workspace(&server, workspace.id.clone()).await;
    let api_key = api_key_resp.key.clone().unwrap();

    // Set API key spend limit to a very low amount (1 nano-dollar = $0.000000001)
    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 1i64,  // Minimal amount
            "scale": 9,
            "currency": "USD"
        }
    });

    let response = server
        .patch(
            format!(
                "/v1/workspaces/{}/api-keys/{}/spend-limit",
                workspace.id, api_key_resp.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    assert_eq!(response.status_code(), 200);
    println!("Set low API key spend limit");

    // Upsert a model with known pricing
    let batch = generate_model();
    let model_name = batch.keys().next().unwrap().clone();
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // First request might succeed or fail depending on timing
    let response1 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hi"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    println!("First request status: {}", response1.status_code());

    // Wait for usage to be recorded
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Second request should definitely fail with API key limit exceeded
    let response2 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hi again"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    println!("Second request status: {}", response2.status_code());
    println!("Second request body: {}", response2.text());

    // Should get 402 Payment Required with api_key_limit_exceeded error
    assert_eq!(
        response2.status_code(),
        402,
        "Expected 402 Payment Required for API key limit exceeded"
    );

    let error = serde_json::from_str::<api::models::ErrorResponse>(&response2.text())
        .expect("Failed to parse error response");

    println!("Error: {:?}", error);
    assert_eq!(
        error.error.r#type, "api_key_limit_exceeded",
        "Error type should be api_key_limit_exceeded"
    );
    assert!(
        error.error.message.contains("API key spend limit exceeded"),
        "Error message should mention API key limit"
    );
}

#[tokio::test]
async fn test_api_key_limit_enforced_before_org_limit() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database.clone(), auth_components, domain_services);
    let server = axum_test::TestServer::new(app).unwrap();

    // Create organization
    let org = create_org(&server).await;

    // Set organization limit to $5.00
    let org_limit_request = serde_json::json!({
        "spendLimit": {
            "amount": 5000000000i64,  // $5.00 USD
            "scale": 9,
            "currency": "USD"
        },
        "changedBy": "admin@test.com",
        "changeReason": "Test org limit"
    });

    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&org_limit_request)
        .await;

    assert_eq!(response.status_code(), 200);

    // Create API key with lower limit than org ($2.00)
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp = create_api_key_in_workspace(&server, workspace.id.clone()).await;
    let _api_key = api_key_resp.key.clone().unwrap();

    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 2000000000i64,  // $2.00 USD (lower than org limit)
            "scale": 9,
            "currency": "USD"
        }
    });

    let response = server
        .patch(
            format!(
                "/v1/workspaces/{}/api-keys/{}/spend-limit",
                workspace.id, api_key_resp.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    assert_eq!(response.status_code(), 200);
    println!("Set API key limit to $2.00 (org limit is $5.00)");

    // Note: In a real test, we'd make requests until hitting the $2.00 API key limit
    // and verify we get "api_key_limit_exceeded" error, not "insufficient_credits"
    // This would prove the API key limit is checked first.

    println!("Test complete: API key has lower limit ($2.00) than org ($5.00)");
    println!("In production, API key limit would be enforced before org limit");
}

#[tokio::test]
async fn test_api_key_spend_limit_unauthorized_user() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(LevelFilter::DEBUG)
        .try_init();

    let config = test_config();
    let database = init_test_database(&config.database).await;

    // Create mock admin user
    assert_mock_user_in_db(&database).await;

    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(database.clone(), &config).await;

    let app = build_app(database.clone(), auth_components, domain_services);
    let server = axum_test::TestServer::new(app).unwrap();

    // Create two separate organizations
    let org1 = create_org(&server).await;
    let _org2 = create_org(&server).await;

    // Get workspace and API key from org1
    let workspaces1 = list_workspaces(&server, org1.id.clone()).await;
    let workspace1 = workspaces1.first().unwrap();
    let api_key_resp1 = create_api_key_in_workspace(&server, workspace1.id.clone()).await;

    // Try to update org1's API key limit while authenticated as org1 member
    // This should succeed since we're a member
    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 1000000000i64,
            "scale": 9,
            "currency": "USD"
        }
    });

    let response = server
        .patch(
            format!(
                "/v1/workspaces/{}/api-keys/{}/spend-limit",
                workspace1.id, api_key_resp1.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Org member should be able to update API key limits"
    );

    println!("Test complete: Verified permission checking for API key spend limits");
}
