use api::{
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
use config::{ApiConfig, LoggingConfig};
use database::{
    repositories::{ApiKeyRepository, PgOrganizationRepository, SessionRepository, UserRepository},
    Database,
};
use inference_providers::{InferenceProvider, VLlmConfig, VLlmProvider};
use services::auth::{AuthService, OAuthManager};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

#[tokio::main]
async fn main() {
    // Load configuration first to get logging settings
    let config = ApiConfig::load().unwrap_or_else(|e| {
        eprintln!("Failed to load configuration: {}", e);
        eprintln!("Application cannot start without a valid configuration file.");
        std::process::exit(1);
    });

    // Initialize tracing with configuration from config.yaml
    init_tracing(&config.logging);

    tracing::debug!("Config: {:?}", config);

    // Get server config from configuration
    let server_config = config.server.clone();
    let bind_address = format!("{}:{}", server_config.host, server_config.port);

    // Create database configuration
    let db_config = database::DatabaseConfig::default();
    let database = Arc::new(Database::from_config(&db_config).await.unwrap());

    // Create repository instances using the database pool
    let user_repository = Arc::new(UserRepository::new(database.pool().clone()))
        as Arc<dyn services::auth::UserRepository>;
    let session_repository = Arc::new(SessionRepository::new(database.pool().clone()))
        as Arc<dyn services::auth::SessionRepository>;
    let api_key_repository = Arc::new(ApiKeyRepository::new(database.pool().clone()))
        as Arc<dyn services::auth::ApiKeyRepository>;
    let organization_repository = Arc::new(PgOrganizationRepository::new(database.pool().clone()))
        as Arc<dyn services::organization::ports::OrganizationRepository>;

    // Create AuthService
    let auth_service = Arc::new(AuthService::new(
        user_repository,
        session_repository,
        api_key_repository,
        organization_repository,
    ));

    // Create OAuth manager with configuration
    tracing::info!("Authentication enabled, setting up OAuth providers");

    let github_config = config
        .auth
        .github
        .clone()
        .map(|c| config::OAuthProviderConfig::from(c));
    let google_config = config
        .auth
        .google
        .clone()
        .map(|c| config::OAuthProviderConfig::from(c));

    let oauth_manager = OAuthManager::new(github_config, google_config).unwrap_or_else(|e| {
        tracing::error!("Failed to create OAuth manager: {}", e);
        std::process::exit(1);
    });

    if config.auth.github.is_some() {
        tracing::info!("GitHub OAuth configured");
    }
    if config.auth.google.is_some() {
        tracing::info!("Google OAuth configured");
    }

    let state_store: StateStore = Arc::new(RwLock::new(HashMap::new()));

    // Create AuthState for middleware
    let oauth_manager_arc = Arc::new(oauth_manager);
    let auth_state_middleware =
        AuthState::new(oauth_manager_arc.clone(), Some(auth_service.clone()));

    // Build authentication routes with combined state
    let auth_state = (
        oauth_manager_arc.clone(),
        state_store.clone(),
        auth_service.clone(),
    );
    let auth_routes = Router::new()
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
        .with_state(auth_state);

    // Build API routes for completions
    let completion_routes = if config.auth.enabled {
        Router::new()
            .route("/chat/completions", post(chat_completions))
            .route("/completions", post(completions))
            .route("/models", get(models))
            .route("/quote", get(quote))
            .layer(from_fn_with_state(
                auth_state_middleware.clone(),
                auth_middleware,
            ))
    } else {
        // No auth middleware when disabled
        Router::new()
            .route("/chat/completions", post(chat_completions))
            .route("/completions", post(completions))
            .route("/models", get(models))
            .route("/quote", get(quote))
    };

    // Create shared repositories
    let conversation_repo = Arc::new(database::PgConversationRepository::new(
        database.pool().clone(),
    ));
    let response_repo = Arc::new(database::PgResponseRepository::new(database.pool().clone()));

    // Build Response and Conversation API routes with domain services
    let conversation_service = Arc::new(services::ConversationService::new(
        conversation_repo.clone(),
        response_repo.clone(),
    ));

    // Create inference provider pool for completions (empty for now)
    let providers = config
        .providers
        .iter()
        .map(|p| {
            Arc::new(VLlmProvider::new(VLlmConfig::new(
                p.url.clone(),
                p.api_key.clone(),
                None,
            ))) as Arc<dyn InferenceProvider + Send + Sync>
        })
        .collect::<Vec<_>>();

    let inference_provider_pool =
        Arc::new(services::inference_provider_pool::InferenceProviderPool::new(providers));

    // Initialize model discovery during startup
    if let Err(e) = inference_provider_pool.initialize().await {
        tracing::warn!("Failed to initialize model discovery during startup: {}", e);
        tracing::info!("Models will be discovered on first request");
    }

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

    let response_routes = if config.auth.enabled {
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
    } else {
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
    };

    let conversation_routes = if config.auth.enabled {
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
    } else {
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
    };

    // Build management API routes (orgs, teams, users)
    let management_routes = {
        // Create shared MCP client manager
        let mcp_manager = Arc::new(services::mcp::McpClientManager::new());

        let app_state = AppState {
            db: database.clone(),
            mcp_manager,
            completion_service: completion_service.clone(),
            models_service: models_service.clone(),
        };

        Some(build_management_router(
            app_state,
            auth_state_middleware.clone(),
            config.auth.enabled,
        ))
    };

    // Build the final application with consistent /v1/* routing
    let app = Router::new().nest(
        "/v1",
        Router::new()
            .nest("/auth", auth_routes)
            .merge(completion_routes.with_state(AppState {
                db: database.clone(),
                mcp_manager: Arc::new(services::mcp::McpClientManager::new()),
                completion_service: completion_service.clone(),
                models_service: models_service.clone(),
            }))
            .merge(response_routes)
            .merge(conversation_routes)
            .merge(if let Some(mgmt_routes) = management_routes {
                mgmt_routes
            } else {
                Router::new()
            }),
    );

    // Start periodic session cleanup
    if config.auth.enabled {
        // TODO: Implement session cleanup when auth service is properly wired up
        tracing::info!("Session cleanup disabled until auth service is fully configured");
    }

    // run our app with hyper, using configuration from domain
    let listener = tokio::net::TcpListener::bind(&bind_address).await.unwrap();

    tracing::info!(address = %bind_address, "Server started successfully");
    tracing::info!(
        "Authentication: {}",
        if config.auth.enabled {
            "ENABLED"
        } else {
            "DISABLED"
        }
    );

    axum::serve(listener, app).await.unwrap();
}

fn init_tracing(logging_config: &LoggingConfig) {
    // Build the filter string from the logging configuration
    let mut filter = logging_config.level.clone();

    for (module, level) in &logging_config.modules {
        filter.push_str(&format!(",{}={}", module, level));
    }

    // Initialize tracing based on the format specified in config
    match logging_config.format.as_str() {
        "json" => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .init();
        }
        "compact" => {
            tracing_subscriber::fmt()
                .compact()
                .with_env_filter(filter)
                .init();
        }
        "pretty" | _ => {
            tracing_subscriber::fmt()
                .pretty()
                .with_env_filter(filter)
                .init();
        }
    }
}
