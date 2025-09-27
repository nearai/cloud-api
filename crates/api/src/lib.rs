pub mod conversions;
pub mod middleware;
pub mod models;
pub mod openapi;
pub mod routes;

use crate::{
    middleware::{auth_middleware, AuthState},
    openapi::ApiDoc,
    routes::{
        api::{build_management_router, AppState},
        attestation::{get_attestation_report, get_signature, verify_attestation},
        auth::{
            auth_success, current_user, github_login, google_login, login_page, logout,
            oauth_callback, StateStore,
        },
        completions::{chat_completions, completions, models, quote},
        conversations, responses,
    },
};
use axum::{
    middleware::from_fn_with_state,
    response::Html,
    routing::{get, post},
    Router,
};
use config::ApiConfig;
use database::{
    repositories::{ApiKeyRepository, PgOrganizationRepository, SessionRepository, UserRepository},
    Database,
};
use inference_providers::{InferenceProvider, VLlmConfig, VLlmProvider};
use services::auth::{AuthService, AuthServiceTrait, MockAuthService, OAuthManager};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use utoipa::OpenApi;

/// Service initialization components
pub struct AuthComponents {
    pub auth_service: Arc<dyn AuthServiceTrait>,
    pub oauth_manager: Arc<OAuthManager>,
    pub state_store: StateStore,
    pub auth_state_middleware: AuthState,
}

#[derive(Clone)]
pub struct DomainServices {
    pub conversation_service: Arc<services::ConversationService>,
    pub response_service: Arc<services::ResponseService>,
    pub completion_service: Arc<services::CompletionServiceImpl>,
    pub models_service: Arc<services::models::ModelsServiceImpl>,
    pub mcp_manager: Arc<services::mcp::McpClientManager>,
    pub inference_provider_pool: Arc<services::inference_provider_pool::InferenceProviderPool>,
    pub attestation_service: Arc<services::attestation::AttestationService>,
}

/// Initialize database connection and run migrations
pub async fn init_database() -> Arc<Database> {
    let db_config = config::DatabaseConfig::default();
    let database = Arc::new(
        Database::from_config(&db_config)
            .await
            .expect("Failed to connect to database"),
    );

    // Run database migrations
    tracing::info!("Starting database migrations...");
    database
        .run_migrations()
        .await
        .expect("Failed to run database migrations");
    tracing::info!("Database migrations completed.");

    database
}

/// Initialize database with custom config for testing
pub async fn init_database_with_config(db_config: &config::DatabaseConfig) -> Arc<Database> {
    let database = Arc::new(
        Database::from_config(db_config)
            .await
            .expect("Failed to connect to database"),
    );

    // Run database migrations
    database
        .run_migrations()
        .await
        .expect("Failed to run database migrations");

    database
}

/// Initialize authentication services and middleware
pub fn init_auth_services(database: Arc<Database>, config: &ApiConfig) -> AuthComponents {
    // Choose auth service implementation based on config
    let auth_service: Arc<dyn AuthServiceTrait> = if config.auth.mock {
        // Use MockAuthService when mock auth is enabled
        Arc::new(MockAuthService)
    } else {
        // Create repository instances
        let user_repository = Arc::new(UserRepository::new(database.pool().clone()))
            as Arc<dyn services::auth::UserRepository>;
        let session_repository = Arc::new(SessionRepository::new(database.pool().clone()))
            as Arc<dyn services::auth::SessionRepository>;
        let api_key_repository = Arc::new(ApiKeyRepository::new(database.pool().clone()))
            as Arc<dyn services::auth::ApiKeyRepository>;
        let organization_repository =
            Arc::new(PgOrganizationRepository::new(database.pool().clone()))
                as Arc<dyn services::organization::ports::OrganizationRepository>;

        // Create AuthService
        Arc::new(AuthService::new(
            user_repository,
            session_repository,
            api_key_repository,
            organization_repository,
        ))
    };

    // Create OAuth manager
    tracing::info!("Setting up OAuth providers");
    let oauth_manager = create_oauth_manager(config);
    let state_store: StateStore = Arc::new(RwLock::new(HashMap::new()));

    // Create AuthState for middleware
    let oauth_manager_arc = Arc::new(oauth_manager);
    let auth_state_middleware = AuthState::new(oauth_manager_arc.clone(), auth_service.clone());

    AuthComponents {
        auth_service,
        oauth_manager: oauth_manager_arc,
        state_store,
        auth_state_middleware,
    }
}

/// Create OAuth manager from configuration
pub fn create_oauth_manager(config: &ApiConfig) -> OAuthManager {
    let github_config = config
        .auth
        .github
        .clone()
        .map(config::OAuthProviderConfig::from);
    let google_config = config
        .auth
        .google
        .clone()
        .map(config::OAuthProviderConfig::from);

    let manager = OAuthManager::new(github_config, google_config).unwrap_or_else(|e| {
        tracing::error!("Failed to create OAuth manager: {}", e);
        std::process::exit(1);
    });

    if config.auth.github.is_some() {
        tracing::info!("GitHub OAuth configured");
    }
    if config.auth.google.is_some() {
        tracing::info!("Google OAuth configured");
    }

    manager
}

/// Initialize domain services
pub async fn init_domain_services(database: Arc<Database>, config: &ApiConfig) -> DomainServices {
    // Create shared repositories
    let conversation_repo = Arc::new(database::PgConversationRepository::new(
        database.pool().clone(),
    ));
    let response_repo = Arc::new(database::PgResponseRepository::new(database.pool().clone()));
    let attestation_repo = Arc::new(database::PgAttestationRepository::new(
        database.pool().clone(),
    ));

    // Create conversation service
    let conversation_service = Arc::new(services::ConversationService::new(
        conversation_repo.clone(),
        response_repo.clone(),
    ));

    // Create inference provider pool
    let inference_provider_pool = init_inference_providers(config).await;

    // Create attestation service
    let attestation_service = Arc::new(services::attestation::AttestationService::new(
        attestation_repo,
        inference_provider_pool.clone(),
    ));

    // Create models service
    let models_service = Arc::new(services::models::ModelsServiceImpl::new(
        inference_provider_pool.clone(),
    ));

    // Create completion service
    let completion_service = Arc::new(services::CompletionServiceImpl::new(
        inference_provider_pool.clone(),
        attestation_service.clone(),
    ));

    // Create response service
    let response_service = Arc::new(services::ResponseService::new(
        response_repo,
        inference_provider_pool.clone(),
        conversation_service.clone(),
    ));

    // Create MCP client manager
    let mcp_manager = Arc::new(services::mcp::McpClientManager::new());

    DomainServices {
        conversation_service,
        response_service,
        completion_service,
        models_service,
        mcp_manager,
        inference_provider_pool,
        attestation_service,
    }
}

/// Initialize inference provider pool
pub async fn init_inference_providers(
    config: &ApiConfig,
) -> Arc<services::inference_provider_pool::InferenceProviderPool> {
    let providers: Vec<Arc<dyn InferenceProvider + Send + Sync>> = config
        .providers
        .iter()
        .map(|p| {
            Arc::new(VLlmProvider::new(VLlmConfig::new(
                p.url.clone(),
                p.api_key.clone(),
                None,
            ))) as Arc<dyn InferenceProvider + Send + Sync>
        })
        .collect();

    let pool = Arc::new(services::inference_provider_pool::InferenceProviderPool::new(providers));

    // Initialize model discovery during startup
    if let Err(e) = pool.initialize().await {
        tracing::warn!("Failed to initialize model discovery during startup: {}", e);
        tracing::info!("Models will be discovered on first request");
    }

    pool
}

/// Build the complete application router
pub fn build_app(
    database: Arc<Database>,
    auth_components: AuthComponents,
    domain_services: DomainServices,
) -> Router {
    build_app_with_config(database, auth_components, domain_services, None)
}

/// Build the complete application router with config
pub fn build_app_with_config(
    database: Arc<Database>,
    auth_components: AuthComponents,
    domain_services: DomainServices,
    _config: Option<&ApiConfig>,
) -> Router {
    // Create organization service using the database's organization repository
    let organization_repo = Arc::new(database::PgOrganizationRepository::new(
        database.pool().clone(),
    ));
    let organization_service = Arc::new(services::organization::OrganizationService::new(
        organization_repo,
    ));

    // Create app state for completions and management routes
    let app_state = AppState {
        db: database.clone(),
        organization_service,
        mcp_manager: domain_services.mcp_manager.clone(),
        completion_service: domain_services.completion_service.clone(),
        models_service: domain_services.models_service.clone(),
        auth_service: auth_components.auth_service.clone(),
        attestation_service: domain_services.attestation_service.clone(),
    };

    // Build individual route groups
    let auth_routes = build_auth_routes(
        auth_components.oauth_manager.clone(),
        auth_components.state_store,
        auth_components.auth_service.clone(),
        &auth_components.auth_state_middleware,
    );

    let completion_routes =
        build_completion_routes(app_state.clone(), &auth_components.auth_state_middleware);

    let response_routes = build_response_routes(
        domain_services.response_service,
        &auth_components.auth_state_middleware,
    );

    let conversation_routes = build_conversation_routes(
        domain_services.conversation_service,
        &auth_components.auth_state_middleware,
    );

    let management_routes = build_management_router(
        app_state.clone(),
        auth_components.auth_state_middleware.clone(),
    );

    let attestation_routes =
        build_attestation_routes(app_state, &auth_components.auth_state_middleware);

    // Build OpenAPI and documentation routes
    let openapi_routes = build_openapi_routes();

    Router::new()
        .nest(
            "/v1",
            Router::new()
                .nest("/auth", auth_routes)
                .merge(completion_routes)
                .merge(response_routes)
                .merge(conversation_routes)
                .merge(management_routes)
                .merge(attestation_routes.clone()),
        )
        .merge(openapi_routes)
}

/// Build authentication routes
pub fn build_auth_routes(
    oauth_manager: Arc<OAuthManager>,
    state_store: StateStore,
    auth_service: Arc<dyn AuthServiceTrait>,
    auth_state_middleware: &AuthState,
) -> Router {
    let auth_state = (oauth_manager, state_store, auth_service);

    Router::new()
        .route("/login", get(login_page))
        .route("/github", get(github_login))
        .route("/google", get(google_login))
        .route("/callback", get(oauth_callback))
        .route(
            "/user",
            get(current_user).layer(from_fn_with_state(
                auth_state_middleware.clone(),
                auth_middleware,
            )),
        )
        .route("/logout", post(logout))
        .route("/success", get(auth_success))
        .with_state(auth_state)
}

/// Build completion routes with auth
pub fn build_completion_routes(app_state: AppState, auth_state_middleware: &AuthState) -> Router {
    Router::new()
        .route("/chat/completions", post(chat_completions))
        .route("/completions", post(completions))
        .route("/models", get(models))
        .route("/quote", get(quote))
        .with_state(app_state)
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            auth_middleware,
        ))
}

/// Build response routes with auth
pub fn build_response_routes(
    response_service: Arc<services::ResponseService>,
    auth_state_middleware: &AuthState,
) -> Router {
    Router::new()
        .route("/responses", post(responses::create_response))
        .route("/responses/{response_id}", get(responses::get_response))
        .route(
            "/responses/{response_id}",
            axum::routing::delete(responses::delete_response),
        )
        .route(
            "/responses/{response_id}/cancel",
            post(responses::cancel_response),
        )
        .route(
            "/responses/{response_id}/input_items",
            get(responses::list_input_items),
        )
        .with_state(response_service)
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            auth_middleware,
        ))
}

/// Build conversation routes with auth
pub fn build_conversation_routes(
    conversation_service: Arc<services::ConversationService>,
    auth_state_middleware: &AuthState,
) -> Router {
    Router::new()
        .route("/conversations", get(conversations::list_conversations))
        .route("/conversations", post(conversations::create_conversation))
        .route(
            "/conversations/{conversation_id}",
            get(conversations::get_conversation),
        )
        .route(
            "/conversations/{conversation_id}",
            post(conversations::update_conversation),
        )
        .route(
            "/conversations/{conversation_id}",
            axum::routing::delete(conversations::delete_conversation),
        )
        .route(
            "/conversations/{conversation_id}/items",
            get(conversations::list_conversation_items),
        )
        .with_state(conversation_service)
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            auth_middleware,
        ))
}

/// Build attestation routes with auth
pub fn build_attestation_routes(app_state: AppState, auth_state_middleware: &AuthState) -> Router {
    Router::new()
        // v1 routes (signature endpoint)
        .route("/signature/{chat_id}", get(get_signature))
        .route("/verify/{chat_id}", post(verify_attestation))
        // api routes (attestation report)
        .route("/attestation/report", get(get_attestation_report))
        .with_state(app_state)
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            auth_middleware,
        ))
}

/// Build OpenAPI documentation routes  
pub fn build_openapi_routes() -> Router {
    Router::new().route("/docs", get(swagger_ui_handler)).route(
        "/api-docs/openapi.json",
        get(|| async { axum::Json(ApiDoc::openapi()) }),
    )
}

/// Serve Swagger UI HTML page
async fn swagger_ui_handler() -> Html<String> {
    Html(r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>Platform API Documentation</title>
    <link rel="stylesheet" type="text/css" href="https://unpkg.com/swagger-ui-dist@5.10.5/swagger-ui.css" />
    <style>
        html {
            box-sizing: border-box;
            overflow: -moz-scrollbars-vertical;
            overflow-y: scroll;
        }
        *, *:before, *:after {
            box-sizing: inherit;
        }
        body {
            margin:0;
            background: #fafafa;
        }
    </style>
</head>
<body>
    <div id="swagger-ui"></div>
    <script src="https://unpkg.com/swagger-ui-dist@5.10.5/swagger-ui-bundle.js"></script>
    <script src="https://unpkg.com/swagger-ui-dist@5.10.5/swagger-ui-standalone-preset.js"></script>
    <script>
    window.onload = function() {
        // Dynamically determine the server URL based on current location
        const protocol = window.location.protocol;
        const host = window.location.host;
        const baseUrl = `${protocol}//${host}/v1`;
        
        // Fetch the OpenAPI spec and modify it to include the dynamic server
        fetch('/api-docs/openapi.json')
            .then(response => response.json())
            .then(spec => {
                // Add the current server to the spec
                spec.servers = [{ 
                    url: baseUrl,
                    description: 'Current Server'
                }];
                
                SwaggerUIBundle({
                    spec: spec,
                    dom_id: '#swagger-ui',
                    deepLinking: true,
                    presets: [
                        SwaggerUIBundle.presets.apis,
                        SwaggerUIStandalonePreset
                    ],
                    plugins: [
                        SwaggerUIBundle.plugins.DownloadUrl
                    ],
                    layout: "StandaloneLayout",
                    // Make authorization more prominent
                    persistAuthorization: true,
                    // Show auth section by default
                    docExpansion: 'list',
                    // Configure request interceptor for debugging
                    requestInterceptor: function(req) {
                        console.log('Swagger UI Request:', req);
                        return req;
                    }
                });
            })
            .catch(error => {
                console.error('Failed to load OpenAPI spec:', error);
                // Fallback to URL-based loading if fetch fails
                SwaggerUIBundle({
                    url: '/api-docs/openapi.json',
                    dom_id: '#swagger-ui',
                    deepLinking: true,
                    presets: [
                        SwaggerUIBundle.presets.apis,
                        SwaggerUIStandalonePreset
                    ],
                    plugins: [
                        SwaggerUIBundle.plugins.DownloadUrl
                    ],
                    layout: "StandaloneLayout",
                    // Make authorization more prominent
                    persistAuthorization: true,
                    // Show auth section by default
                    docExpansion: 'list',
                    // Configure request interceptor for debugging
                    requestInterceptor: function(req) {
                        console.log('Swagger UI Request:', req);
                        return req;
                    }
                });
            });
    };
    </script>
</body>
</html>"#.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openapi::ApiDoc;

    #[test]
    fn test_openapi_spec_generation() {
        // Test that we can generate the OpenAPI spec without errors
        let spec = ApiDoc::openapi();

        // Basic validation
        assert_eq!(spec.info.title, "Platform API");
        assert_eq!(spec.info.version, "1.0.0");

        // Ensure we have components defined
        assert!(spec.components.is_some());
        let components = spec.components.as_ref().unwrap();

        // Check that some of our schemas are present
        assert!(components.schemas.contains_key("ChatCompletionRequest"));
        assert!(components.schemas.contains_key("ChatCompletionResponse"));
        assert!(components.schemas.contains_key("Message"));
        assert!(components.schemas.contains_key("ModelsResponse"));
        assert!(components.schemas.contains_key("ErrorResponse"));

        // Check that security schemes are configured
        assert!(components.security_schemes.contains_key("bearer"));
        assert!(components.security_schemes.contains_key("api_key"));

        // Verify servers are not hardcoded (will be set dynamically on client)
        assert!(spec.servers.is_none() || spec.servers.as_ref().unwrap().is_empty());
    }

    #[test]
    fn test_swagger_ui_html_contains_required_elements() {
        // Test that the Swagger UI HTML contains the necessary elements
        use axum::response::Html;

        // Get the HTML response
        let html = tokio_test::block_on(swagger_ui_handler());
        let Html(html_content) = html;

        // Verify essential Swagger UI elements are present
        assert!(
            html_content.contains("swagger-ui"),
            "HTML should contain swagger-ui div"
        );
        assert!(
            html_content.contains("swagger-ui-bundle.js"),
            "HTML should include Swagger UI bundle"
        );
        assert!(
            html_content.contains("swagger-ui-standalone-preset.js"),
            "HTML should include standalone preset"
        );
        assert!(
            html_content.contains("/api-docs/openapi.json"),
            "HTML should reference our OpenAPI spec URL"
        );
        assert!(
            html_content.contains("Platform API Documentation"),
            "HTML should have the correct title"
        );
        assert!(
            html_content.contains("SwaggerUIBundle"),
            "HTML should initialize SwaggerUIBundle"
        );
    }

    /// Example of how to set up the application for E2E testing
    #[tokio::test]
    #[ignore] // Remove ignore to run with a real database
    async fn test_app_setup() {
        // Create a test configuration
        let config = ApiConfig {
            providers: vec![],
            server: config::ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 0, // Use port 0 for testing to get a random available port
            },
            model_discovery: config::ModelDiscoveryConfig {
                refresh_interval: 0,
                timeout: 5,
            },
            logging: config::LoggingConfig {
                level: "info".to_string(),
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
            database: config::DatabaseConfig {
                host: "localhost".to_string(),
                port: 5432,
                database: "test_db".to_string(),
                username: "test_user".to_string(),
                password: "test_pass".to_string(),
                max_connections: 5,
            },
        };

        // Initialize services
        let database = init_database().await;
        let auth_components = init_auth_services(database.clone(), &config);
        let domain_services = init_domain_services(database.clone(), &config).await;

        // Build the application
        let _app = build_app(database, auth_components, domain_services);

        // You can now use `app` with a test server like:
        // let server = axum_test::TestServer::new(app).unwrap();
        // let response = server.get("/v1/models").await;
        // assert_eq!(response.status(), 200);
    }

    /// Example of testing with custom database configuration
    #[tokio::test]
    #[ignore] // Remove ignore when you have a test database
    async fn test_with_custom_database() {
        // Create custom database config for testing
        let db_config = config::DatabaseConfig {
            host: "localhost".to_string(),
            port: 5432,
            database: "test_db".to_string(),
            username: "test_user".to_string(),
            password: "test_pass".to_string(),
            max_connections: 5,
        };

        // Initialize database with custom config
        let database = init_database_with_config(&db_config).await;

        // Create a test configuration
        let config = ApiConfig {
            providers: vec![],
            server: config::ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
            },
            model_discovery: config::ModelDiscoveryConfig {
                refresh_interval: 0,
                timeout: 5,
            },
            logging: config::LoggingConfig {
                level: "info".to_string(),
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
            database: config::DatabaseConfig {
                host: "localhost".to_string(),
                port: 5432,
                database: "test_db".to_string(),
                username: "test_user".to_string(),
                password: "test_pass".to_string(),
                max_connections: 5,
            },
        };

        let auth_components = init_auth_services(database.clone(), &config);
        let domain_services = init_domain_services(database.clone(), &config).await;

        let _app = build_app(database, auth_components, domain_services);

        // Test the app...
    }
}
