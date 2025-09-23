pub mod conversions;
pub mod middleware;
pub mod models;
pub mod routes;

use crate::{
    middleware::{auth_middleware, AuthState},
    routes::{
        api::{build_management_router, AppState},
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

/// Service initialization components
pub struct AuthComponents {
    pub auth_service: Arc<dyn AuthServiceTrait>,
    pub oauth_manager: Arc<OAuthManager>,
    pub state_store: StateStore,
    pub auth_state_middleware: AuthState,
}

pub struct DomainServices {
    pub conversation_service: Arc<services::ConversationService>,
    pub response_service: Arc<services::ResponseService>,
    pub completion_service: Arc<services::CompletionServiceImpl>,
    pub models_service: Arc<services::models::ModelsServiceImpl>,
    pub mcp_manager: Arc<services::mcp::McpClientManager>,
}

/// Initialize database connection
pub async fn init_database() -> Arc<Database> {
    let db_config = config::DatabaseConfig::default();
    Arc::new(
        Database::from_config(&db_config)
            .await
            .expect("Failed to connect to database"),
    )
}

/// Initialize database with custom config for testing
pub async fn init_database_with_config(db_config: &config::DatabaseConfig) -> Arc<Database> {
    Arc::new(
        Database::from_config(db_config)
            .await
            .expect("Failed to connect to database"),
    )
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

    // Create conversation service
    let conversation_service = Arc::new(services::ConversationService::new(
        conversation_repo.clone(),
        response_repo.clone(),
    ));

    // Create inference provider pool
    let inference_provider_pool = init_inference_providers(config).await;

    // Create models service
    let models_service = Arc::new(services::models::ModelsServiceImpl::new(
        inference_provider_pool.clone(),
    ));

    // Create completion service
    let completion_service = Arc::new(services::CompletionServiceImpl::new(
        inference_provider_pool.clone(),
    ));

    // Create response service
    let response_service = Arc::new(services::ResponseService::new(
        response_repo,
        inference_provider_pool,
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

    let management_routes =
        build_management_router(app_state, auth_components.auth_state_middleware);

    // Combine all routes under /v1
    Router::new().nest(
        "/v1",
        Router::new()
            .nest("/auth", auth_routes)
            .merge(completion_routes)
            .merge(response_routes)
            .merge(conversation_routes)
            .merge(management_routes),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

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
