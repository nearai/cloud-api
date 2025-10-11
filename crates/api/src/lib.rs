pub mod conversions;
pub mod middleware;
pub mod models;
pub mod openapi;
pub mod routes;

use crate::{
    middleware::{auth::auth_middleware_with_api_key, auth_middleware, AuthState},
    openapi::ApiDoc,
    routes::{
        api::{build_management_router, AppState},
        attestation::{get_attestation_report, get_signature, quote, verify_attestation},
        auth::{
            current_user, github_login, google_login, login_page, logout, oauth_callback,
            StateStore,
        },
        completions::{chat_completions, completions, models},
        conversations,
        models::{get_model_by_name, list_models, ModelsAppState},
        responses,
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
    repositories::{
        ApiKeyRepository, PgOrganizationRepository, SessionRepository, UserRepository,
        WorkspaceRepository,
    },
    Database,
};
use services::{
    auth::{AuthService, AuthServiceTrait, MockAuthService, OAuthManager},
    models::ModelsServiceTrait,
};
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
    pub organization_service:
        Arc<dyn services::organization::OrganizationServiceTrait + Send + Sync>,
    pub workspace_service: Arc<dyn services::workspace::WorkspaceServiceTrait + Send + Sync>,
    pub usage_service: Arc<dyn services::usage::UsageServiceTrait + Send + Sync>,
    pub user_service: Arc<dyn services::user::UserServiceTrait + Send + Sync>,
}

/// Initialize database connection and run migrations
pub async fn init_database(db_config: &config::DatabaseConfig) -> Arc<Database> {
    let database = Arc::new(
        Database::from_config(db_config)
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
    let auth_service: Arc<dyn AuthServiceTrait> = if config.auth.mock {
        // TODO: fix this, it should not use the database pool
        println!("config: {:?}", config);
        Arc::new(MockAuthService {
            apikey_repository: Arc::new(ApiKeyRepository::new(database.pool().clone())),
        })
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

        // Create AuthService with workspace repository
        let workspace_repository_for_auth =
            Arc::new(WorkspaceRepository::new(database.pool().clone()))
                as Arc<dyn services::auth::ports::WorkspaceRepository>;

        Arc::new(AuthService::new(
            user_repository,
            session_repository,
            api_key_repository,
            organization_repository,
            workspace_repository_for_auth,
        ))
    };

    // Create workspace repository
    let workspace_repository = Arc::new(WorkspaceRepository::new(database.pool().clone()))
        as Arc<dyn services::auth::ports::WorkspaceRepository>;

    // Create OAuth manager
    tracing::info!("Setting up OAuth providers");
    let oauth_manager = create_oauth_manager(config);
    let state_store: StateStore = Arc::new(RwLock::new(HashMap::new()));

    // Create AuthState for middleware
    let oauth_manager_arc = Arc::new(oauth_manager);
    let auth_state_middleware = AuthState::new(
        oauth_manager_arc.clone(),
        auth_service.clone(),
        workspace_repository.clone(),
        config.auth.admin_domains.clone(),
    );

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
    let organization_repo = Arc::new(database::PgOrganizationRepository::new(
        database.pool().clone(),
    ));
    let user_repo = Arc::new(database::UserRepository::new(database.pool().clone()))
        as Arc<dyn services::auth::UserRepository>;
    let attestation_repo = Arc::new(database::PgAttestationRepository::new(
        database.pool().clone(),
    ));
    let models_repo = Arc::new(database::repositories::ModelRepository::new(
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
        models_repo.clone(),
    ));

    // Prepare repositories for usage service (will be created after workspace service)
    let usage_repository = Arc::new(database::repositories::OrganizationUsageRepository::new(
        database.pool().clone(),
    ));
    let limits_repository_for_usage = Arc::new(
        database::repositories::OrganizationLimitsRepository::new(database.pool().clone()),
    );

    // Create response service
    let response_service = Arc::new(services::ResponseService::new(
        response_repo,
        inference_provider_pool.clone(),
        conversation_service.clone(),
    ));

    // Create MCP client manager
    let mcp_manager = Arc::new(services::mcp::McpClientManager::new());

    let invitation_repo = Arc::new(database::PgOrganizationInvitationRepository::new(
        database.pool().clone(),
    ))
        as Arc<dyn services::organization::ports::OrganizationInvitationRepository>;
    let organization_service = Arc::new(services::organization::OrganizationServiceImpl::new(
        organization_repo.clone(),
        user_repo.clone(),
        invitation_repo,
    ));

    // Create workspace service with API key management (needs organization_service)
    let workspace_repository = Arc::new(database::repositories::WorkspaceRepository::new(
        database.pool().clone(),
    )) as Arc<dyn services::workspace::WorkspaceRepository>;

    let api_key_repository = Arc::new(database::repositories::ApiKeyRepository::new(
        database.pool().clone(),
    )) as Arc<dyn services::workspace::ApiKeyRepository>;

    let workspace_service = Arc::new(services::workspace::WorkspaceServiceImpl::new(
        workspace_repository,
        api_key_repository,
        organization_service.clone(),
    ))
        as Arc<dyn services::workspace::WorkspaceServiceTrait + Send + Sync>;

    // Now create usage service with workspace_service
    let usage_service = Arc::new(services::usage::UsageServiceImpl::new(
        usage_repository as Arc<dyn services::usage::UsageRepository>,
        models_repo.clone() as Arc<dyn services::usage::ModelRepository>,
        limits_repository_for_usage as Arc<dyn services::usage::OrganizationLimitsRepository>,
        workspace_service.clone(),
    )) as Arc<dyn services::usage::UsageServiceTrait + Send + Sync>;

    // Create completion service with usage tracking (needs usage_service)
    let completion_service = Arc::new(services::CompletionServiceImpl::new(
        inference_provider_pool.clone(),
        attestation_service.clone(),
        usage_service.clone(),
        models_repo.clone() as Arc<dyn services::models::ModelsRepository>,
    ));

    // Create session repository for user service
    let session_repo = Arc::new(database::SessionRepository::new(database.pool().clone()))
        as Arc<dyn services::auth::SessionRepository>;

    // Create workspace repository for user service
    let workspace_repository_for_user = Arc::new(database::repositories::WorkspaceRepository::new(
        database.pool().clone(),
    )) as Arc<dyn services::workspace::WorkspaceRepository>;

    // Create user service (needs organization_service and workspace_service for quick_setup)
    let user_service = Arc::new(services::user::UserService::new(
        user_repo,
        session_repo,
        organization_service.clone(),
        workspace_repository_for_user,
        workspace_service.clone(),
    )) as Arc<dyn services::user::UserServiceTrait + Send + Sync>;

    DomainServices {
        conversation_service,
        response_service,
        completion_service,
        models_service,
        mcp_manager,
        inference_provider_pool,
        attestation_service,
        organization_service,
        workspace_service,
        usage_service,
        user_service,
    }
}

/// Initialize inference provider pool
pub async fn init_inference_providers(
    config: &ApiConfig,
) -> Arc<services::inference_provider_pool::InferenceProviderPool> {
    let discovery_url = config.model_discovery.discovery_server_url.clone();
    let api_key = config.model_discovery.api_key.clone();

    // Create pool with discovery URL and API key
    let pool = Arc::new(
        services::inference_provider_pool::InferenceProviderPool::new(
            discovery_url,
            api_key,
            config.model_discovery.timeout,
        ),
    );

    // Initialize model discovery during startup
    if let Err(e) = pool.initialize().await {
        tracing::warn!("Failed to initialize model discovery during startup: {}", e);
        tracing::info!("Models will be discovered on first request");
    }

    // Start periodic refresh task
    let pool_clone = pool.clone();
    let refresh_interval = config.model_discovery.refresh_interval;

    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(refresh_interval));
        loop {
            interval.tick().await;
            tracing::debug!("Running periodic model discovery refresh");
            // Re-run model discovery
            if let Err(e) = pool_clone.initialize().await {
                tracing::error!("Failed to refresh model discovery: {}", e);
            }
        }
    });

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
    // Create app state for completions and management routes
    let app_state = AppState {
        db: database.clone(),
        organization_service: domain_services.organization_service.clone(),
        workspace_service: domain_services.workspace_service.clone(),
        mcp_manager: domain_services.mcp_manager.clone(),
        completion_service: domain_services.completion_service.clone(),
        models_service: domain_services.models_service.clone(),
        auth_service: auth_components.auth_service.clone(),
        attestation_service: domain_services.attestation_service.clone(),
        usage_service: domain_services.usage_service.clone(),
        user_service: domain_services.user_service.clone(),
    };

    // Create usage state for middleware
    let usage_repository = Arc::new(database::repositories::OrganizationUsageRepository::new(
        database.pool().clone(),
    ));
    let api_key_repository = Arc::new(database::repositories::ApiKeyRepository::new(
        database.pool().clone(),
    ));

    let usage_state = middleware::UsageState {
        usage_service: domain_services.usage_service.clone(),
        usage_repository,
        api_key_repository,
    };

    // Build individual route groups
    let auth_routes = build_auth_routes(
        auth_components.oauth_manager.clone(),
        auth_components.state_store,
        auth_components.auth_service.clone(),
        &auth_components.auth_state_middleware,
    );

    let completion_routes = build_completion_routes(
        app_state.clone(),
        &auth_components.auth_state_middleware,
        usage_state,
    );

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

    let workspace_routes =
        build_workspace_routes(app_state.clone(), &auth_components.auth_state_middleware);

    let attestation_routes =
        build_attestation_routes(app_state.clone(), &auth_components.auth_state_middleware);

    let model_routes = build_model_routes(domain_services.models_service.clone());

    let admin_routes = build_admin_routes(database.clone(), &auth_components.auth_state_middleware);

    let invitation_routes =
        build_invitation_routes(app_state.clone(), &auth_components.auth_state_middleware);

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
                .merge(workspace_routes)
                .merge(attestation_routes.clone())
                .merge(model_routes)
                .merge(admin_routes)
                .merge(invitation_routes),
        )
        .merge(openapi_routes)
}

/// Build invitation routes with selective auth
pub fn build_invitation_routes(app_state: AppState, auth_state_middleware: &AuthState) -> Router {
    use crate::routes::users::{accept_invitation_by_token, get_invitation_by_token};
    use axum::routing::{get, post};

    Router::new().nest(
        "/invitations",
        Router::new()
            // Public route - no auth required to view invitation
            .route("/{token}", get(get_invitation_by_token))
            // Auth required to accept
            .route(
                "/{token}/accept",
                post(accept_invitation_by_token).layer(from_fn_with_state(
                    auth_state_middleware.clone(),
                    auth_middleware,
                )),
            )
            .with_state(app_state),
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
        .with_state(auth_state)
}

/// Build completion routes with auth and usage tracking
pub fn build_completion_routes(
    app_state: AppState,
    auth_state_middleware: &AuthState,
    usage_state: middleware::UsageState,
) -> Router {
    // Routes that require credits (actual inference)
    let inference_routes = Router::new()
        .route("/chat/completions", post(chat_completions))
        .route("/completions", post(completions))
        .with_state(app_state.clone())
        // First check usage limits for inference endpoints
        .layer(from_fn_with_state(
            usage_state,
            middleware::usage_check_middleware,
        ))
        // Then authenticate with workspace context (provides organization)
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            middleware::auth::auth_middleware_with_workspace_context,
        ));

    // Routes that don't require credits (metadata)
    let metadata_routes = Router::new()
        .route("/models", get(models))
        .with_state(app_state)
        // Only require API key, no usage check
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            auth_middleware_with_api_key,
        ));

    // Merge routes
    Router::new().merge(inference_routes).merge(metadata_routes)
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
            auth_middleware_with_api_key,
        ))
}

/// Build conversation routes with auth
pub fn build_conversation_routes(
    conversation_service: Arc<services::ConversationService>,
    auth_state_middleware: &AuthState,
) -> Router {
    Router::new()
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
            auth_middleware_with_api_key,
        ))
}

/// Build attestation routes with auth
pub fn build_attestation_routes(app_state: AppState, auth_state_middleware: &AuthState) -> Router {
    Router::new()
        .route("/signature/{chat_id}", get(get_signature))
        .route("/verify/{chat_id}", post(verify_attestation))
        .route("/attestation/report", get(get_attestation_report))
        .route("/attestation/quote", get(quote))
        .with_state(app_state)
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            auth_middleware_with_api_key,
        ))
}

/// Build workspace routes with auth
pub fn build_workspace_routes(app_state: AppState, auth_state_middleware: &AuthState) -> Router {
    use crate::routes::workspaces::*;

    Router::new()
        // Workspace management routes
        .route(
            "/organizations/{org_id}/workspaces",
            get(list_organization_workspaces).post(create_workspace),
        )
        .route(
            "/workspaces/{workspace_id}",
            get(get_workspace)
                .put(update_workspace)
                .delete(delete_workspace),
        )
        // Workspace API key management
        .route(
            "/workspaces/{workspace_id}/api-keys",
            get(list_workspace_api_keys).post(create_workspace_api_key),
        )
        .route(
            "/workspaces/{workspace_id}/api-keys/{key_id}",
            axum::routing::delete(revoke_workspace_api_key).patch(update_workspace_api_key),
        )
        .route(
            "/workspaces/{workspace_id}/api-keys/{key_id}/spend-limit",
            axum::routing::patch(update_api_key_spend_limit),
        )
        .route(
            "/workspaces/{workspace_id}/api-keys/{key_id}/usage/history",
            get(crate::routes::usage::get_api_key_usage_history),
        )
        .with_state(app_state)
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            auth_middleware,
        ))
}

/// Build model routes (public endpoints)
pub fn build_model_routes(models_service: Arc<dyn ModelsServiceTrait>) -> Router {
    let models_app_state = ModelsAppState { models_service };

    Router::new()
        // Public endpoints - no auth required
        .route("/model/list", get(list_models))
        .route("/model/{model_name}", get(get_model_by_name))
        .with_state(models_app_state)
}

/// Build admin routes (authenticated endpoints)
pub fn build_admin_routes(database: Arc<Database>, auth_state_middleware: &AuthState) -> Router {
    use crate::middleware::admin_middleware;
    use crate::routes::admin::{
        batch_upsert_models, delete_model, get_model_pricing_history, list_users,
        update_organization_limits, AdminAppState,
    };
    use database::repositories::AdminCompositeRepository;
    use services::admin::AdminServiceImpl;

    // Create composite admin repository (handles models, organization limits, and users)
    let admin_repository = Arc::new(AdminCompositeRepository::new(database.pool().clone()));

    // Create admin service with composite repository
    let admin_service = Arc::new(AdminServiceImpl::new(
        admin_repository as Arc<dyn services::admin::AdminRepository>,
    )) as Arc<dyn services::admin::AdminService + Send + Sync>;

    let admin_app_state = AdminAppState { admin_service };

    Router::new()
        .route("/admin/models", axum::routing::patch(batch_upsert_models))
        .route(
            "/admin/models/{model_name}",
            axum::routing::delete(delete_model),
        )
        .route(
            "/admin/models/{model_name}/pricing-history",
            axum::routing::get(get_model_pricing_history),
        )
        .route(
            "/admin/organizations/{org_id}/limits",
            axum::routing::patch(update_organization_limits),
        )
        .route("/admin/users", axum::routing::get(list_users))
        .with_state(admin_app_state)
        // Admin middleware handles both authentication and authorization
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            admin_middleware,
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
    <title>NEAR AI Cloud API Documentation</title>
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
        assert_eq!(spec.info.title, "NEAR AI Cloud API");
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
        assert!(components.security_schemes.contains_key("session_token"));
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
            html_content.contains("NEAR AI Cloud API Documentation"),
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
            server: config::ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 0, // Use port 0 for testing to get a random available port
            },
            model_discovery: config::ModelDiscoveryConfig {
                discovery_server_url: "http://localhost:8080/models".to_string(),
                api_key: Some("test-key".to_string()),
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
                admin_domains: vec![],
            },
            database: config::DatabaseConfig {
                host: "localhost".to_string(),
                port: 5432,
                database: "test_db".to_string(),
                username: "test_user".to_string(),
                password: "test_pass".to_string(),
                max_connections: 5,
                tls_enabled: false,
                tls_ca_cert_path: None,
            },
        };

        // Initialize services
        let database = init_database(&config.database).await;
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
            tls_enabled: false,
            tls_ca_cert_path: None,
        };

        // Initialize database with custom config
        let database = init_database_with_config(&db_config).await;

        // Create a test configuration
        let config = ApiConfig {
            server: config::ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
            },
            model_discovery: config::ModelDiscoveryConfig {
                discovery_server_url: "http://localhost:8080/models".to_string(),
                api_key: Some("test-key".to_string()),
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
                admin_domains: vec![],
            },
            database: config::DatabaseConfig {
                host: "localhost".to_string(),
                port: 5432,
                database: "test_db".to_string(),
                username: "test_user".to_string(),
                password: "test_pass".to_string(),
                max_connections: 5,
                tls_enabled: false,
                tls_ca_cert_path: None,
            },
        };

        let auth_components = init_auth_services(database.clone(), &config);
        let domain_services = init_domain_services(database.clone(), &config).await;

        let _app = build_app(database, auth_components, domain_services);

        // Test the app...
    }
}
