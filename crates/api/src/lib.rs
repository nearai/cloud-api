pub mod consts;
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
        attestation::{get_attestation_report, get_signature},
        auth::{
            current_user, github_login, google_login, login_page, logout, oauth_callback,
            StateStore,
        },
        billing::{get_billing_costs, BillingRouteState},
        completions::{chat_completions, models},
        conversations,
        health::health_check,
        models::{get_model_by_name, list_models, ModelsAppState},
        responses,
    },
};
use axum::http::HeaderValue;
use axum::{
    extract::DefaultBodyLimit,
    middleware::{from_fn, from_fn_with_state},
    response::Html,
    routing::{get, post},
    Router,
};
use config::ApiConfig;
use database::{
    repositories::{
        AdminAccessTokenRepository, ApiKeyRepository, PgOrganizationRepository, SessionRepository,
        UserRepository, WorkspaceRepository,
    },
    Database,
};
use services::{
    auth::{AuthService, AuthServiceTrait, MockAuthService, OAuthManager},
    models::ModelsServiceTrait,
};
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use utoipa::OpenApi;

/// Service initialization components
#[derive(Clone)]
pub struct AuthComponents {
    pub auth_service: Arc<dyn AuthServiceTrait>,
    pub oauth_manager: Arc<OAuthManager>,
    pub state_store: StateStore,
    pub auth_state_middleware: AuthState,
    pub organization_service:
        Arc<dyn services::organization::OrganizationServiceTrait + Send + Sync>,
    pub near_auth_service: Arc<services::auth::NearAuthService>,
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
    pub files_service: Arc<dyn services::files::FileServiceTrait + Send + Sync>,
    pub metrics_service: Arc<dyn services::metrics::MetricsServiceTrait>,
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

    // Start periodic pool status logging
    let pool = database.pool().clone();
    tokio::spawn(async move {
        use std::time::Duration;
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            tracing::info!(
                pool = "database",
                size = pool.status().size,
                available = pool.status().available,
                waiting = pool.status().waiting,
                "Pool status"
            );
        }
    });

    database
}

/// Initialize authentication services and middleware
pub fn init_auth_services(database: Arc<Database>, config: &ApiConfig) -> AuthComponents {
    // Create organization-related repositories first (needed for organization_service)
    let organization_repo = Arc::new(PgOrganizationRepository::new(database.pool().clone()));
    let user_repository = Arc::new(UserRepository::new(database.pool().clone()))
        as Arc<dyn services::auth::UserRepository>;
    let invitation_repo = Arc::new(database::PgOrganizationInvitationRepository::new(
        database.pool().clone(),
    ))
        as Arc<dyn services::organization::ports::OrganizationInvitationRepository>;

    // Create organization service early (needed by AuthService)
    let organization_service = Arc::new(services::organization::OrganizationServiceImpl::new(
        organization_repo.clone() as Arc<dyn services::organization::ports::OrganizationRepository>,
        user_repository.clone(),
        invitation_repo,
    ))
        as Arc<dyn services::organization::OrganizationServiceTrait + Send + Sync>;

    let auth_service: Arc<dyn AuthServiceTrait> = if config.auth.mock {
        // TODO: fix this, it should not use the database pool
        println!("config: {config:?}");
        Arc::new(MockAuthService {
            apikey_repository: Arc::new(ApiKeyRepository::new(database.pool().clone())),
        })
    } else {
        // Create repository instances
        let session_repository = Arc::new(SessionRepository::new(database.pool().clone()))
            as Arc<dyn services::auth::SessionRepository>;
        let api_key_repository = Arc::new(ApiKeyRepository::new(database.pool().clone()))
            as Arc<dyn services::workspace::ApiKeyRepository>;

        // Create AuthService with workspace repository
        let workspace_repository_for_auth =
            Arc::new(WorkspaceRepository::new(database.pool().clone()))
                as Arc<dyn services::workspace::WorkspaceRepository>;

        Arc::new(AuthService::new(
            user_repository,
            session_repository,
            api_key_repository,
            organization_repo as Arc<dyn services::organization::ports::OrganizationRepository>,
            workspace_repository_for_auth,
            organization_service.clone(),
        ))
    };

    // Create workspace repository
    let workspace_repository = Arc::new(WorkspaceRepository::new(database.pool().clone()))
        as Arc<dyn services::workspace::WorkspaceRepository>;

    // Create OAuth manager and state repository
    tracing::info!("Setting up OAuth providers");
    let oauth_manager = create_oauth_manager(config);

    // Create OAuth state repository for cross-instance OAuth state sharing
    let oauth_state_repository = Arc::new(database::repositories::OAuthStateRepository::new(
        database.pool().clone(),
    ));
    let state_store: StateStore = oauth_state_repository;

    // Create admin access token repository
    let admin_access_token_repository =
        Arc::new(AdminAccessTokenRepository::new(database.pool().clone()));

    // Create AuthState for middleware
    let oauth_manager_arc = Arc::new(oauth_manager);
    let auth_state_middleware = AuthState::new(
        oauth_manager_arc.clone(),
        auth_service.clone(),
        workspace_repository.clone(),
        admin_access_token_repository,
        config.auth.admin_domains.clone(),
        config.auth.encoding_key.clone(),
    );

    // Create NEAR nonce repository
    let nonce_repository = Arc::new(database::PostgresNearNonceRepository::new(
        database.pool().clone(),
    )) as Arc<dyn services::auth::NearNonceRepository>;

    // Create NEAR auth service (injecting AuthService for reuse)
    let near_auth_service = Arc::new(services::auth::NearAuthService::new(
        auth_service.clone(),
        nonce_repository,
        config.auth.near.clone(),
    ));

    AuthComponents {
        auth_service,
        oauth_manager: oauth_manager_arc,
        state_store,
        auth_state_middleware,
        organization_service,
        near_auth_service,
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

    let manager = OAuthManager::new(github_config, google_config).unwrap_or_else(|_| {
        tracing::error!("Failed to create OAuth manager");
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
pub async fn init_domain_services(
    database: Arc<Database>,
    config: &ApiConfig,
    organization_service: Arc<dyn services::organization::OrganizationServiceTrait + Send + Sync>,
    metrics_service: Arc<dyn services::metrics::MetricsServiceTrait>,
) -> DomainServices {
    let inference_provider_pool = init_inference_providers(config).await;
    init_domain_services_with_pool(
        database,
        config,
        organization_service,
        inference_provider_pool,
        metrics_service,
    )
    .await
}

/// Initialize domain services with a provided inference provider pool
/// This allows tests to inject mock providers without changing core implementations
pub async fn init_domain_services_with_pool(
    database: Arc<Database>,
    config: &ApiConfig,
    organization_service: Arc<dyn services::organization::OrganizationServiceTrait + Send + Sync>,
    inference_provider_pool: Arc<services::inference_provider_pool::InferenceProviderPool>,
    metrics_service: Arc<dyn services::metrics::MetricsServiceTrait>,
) -> DomainServices {
    // Create shared repositories
    let conversation_repo = Arc::new(database::PgConversationRepository::new(
        database.pool().clone(),
    ));
    let response_repo = Arc::new(database::PgResponseRepository::new(database.pool().clone()));
    let response_items_repo = Arc::new(database::PgResponseItemsRepository::new(
        database.pool().clone(),
    ))
        as Arc<dyn services::responses::ports::ResponseItemRepositoryTrait>;
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
        response_items_repo.clone(),
    ));

    // Create attestation service
    let attestation_service = Arc::new(
        services::attestation::AttestationService::init(
            attestation_repo,
            inference_provider_pool.clone(),
            models_repo.clone(),
            metrics_service.clone(),
        )
        .await,
    );

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

    // Create MCP client manager
    let mcp_manager = Arc::new(services::mcp::McpClientManager::new());

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
        metrics_service.clone(),
    )) as Arc<dyn services::usage::UsageServiceTrait + Send + Sync>;

    // Create completion service with usage tracking (needs usage_service)
    let completion_service = Arc::new(services::CompletionServiceImpl::new(
        inference_provider_pool.clone(),
        attestation_service.clone(),
        usage_service.clone(),
        metrics_service.clone(),
        models_repo.clone() as Arc<dyn services::models::ModelsRepository>,
    ));

    let web_search_provider =
        Arc::new(services::responses::tools::brave::BraveWebSearchProvider::new());

    // Create session repository for user service
    let session_repo = Arc::new(database::SessionRepository::new(database.pool().clone()))
        as Arc<dyn services::auth::SessionRepository>;

    // Create user service
    let user_service = Arc::new(services::user::UserService::new(user_repo, session_repo))
        as Arc<dyn services::user::UserServiceTrait + Send + Sync>;

    // Create S3 storage and file service (must be created before response service)
    let s3_storage: Arc<dyn services::files::storage::StorageTrait> = if config.s3.mock {
        tracing::info!("Using mock S3 storage for file uploads");
        Arc::new(services::files::storage::MockStorage::new(
            config.s3.encryption_key.clone(),
        ))
    } else {
        tracing::info!("Using real S3 storage for file uploads");
        let s3_config = aws_config::load_from_env().await;
        let s3_client = aws_sdk_s3::Client::new(&s3_config);

        Arc::new(services::files::storage::S3Storage::new(
            s3_client,
            config.s3.bucket.clone(),
            config.s3.encryption_key.clone(),
        ))
    };

    let file_repository = Arc::new(database::repositories::FileRepository::new(
        database.pool().clone(),
    )) as Arc<dyn services::files::FileRepositoryTrait>;

    let files_service = Arc::new(services::files::FileServiceImpl::new(
        file_repository,
        s3_storage,
    )) as Arc<dyn services::files::FileServiceTrait + Send + Sync>;

    let response_service = Arc::new(services::ResponseService::new(
        response_repo,
        response_items_repo.clone(),
        inference_provider_pool.clone(),
        conversation_service.clone(),
        completion_service.clone(),
        Some(web_search_provider), // web_search_provider
        None,                      // file_search_provider
        files_service.clone(),     // file_service
        organization_service.clone(),
    ));

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
        files_service,
        metrics_service,
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
            config.model_discovery.inference_timeout,
        ),
    );

    // Initialize model discovery during startup
    if pool.initialize().await.is_err() {
        tracing::warn!("Failed to initialize model discovery during startup");
        tracing::info!("Models will be discovered on first request");
    }

    // Start periodic refresh task with handle management
    let refresh_interval = config.model_discovery.refresh_interval as u64;
    pool.clone().start_refresh_task(refresh_interval).await;

    pool
}

/// Initialize inference provider pool with mock providers for testing
/// This function uses the existing MockProvider from inference_providers::mock
/// and registers it for common test models without changing any implementations
pub async fn init_inference_providers_with_mocks(
    _config: &ApiConfig,
) -> (
    Arc<services::inference_provider_pool::InferenceProviderPool>,
    Arc<inference_providers::mock::MockProvider>,
) {
    use inference_providers::MockProvider;
    use std::sync::Arc;

    // Create pool with dummy discovery URL (won't be used since we're registering providers directly)
    let pool = Arc::new(
        services::inference_provider_pool::InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            30 * 60,
        ),
    );

    // Create a MockProvider that accepts all models (using new_accept_all)
    let mock_provider = Arc::new(MockProvider::new_accept_all());
    let mock_provider_trait: Arc<dyn inference_providers::InferenceProvider + Send + Sync> =
        mock_provider.clone();

    // Register providers for models commonly used in tests
    let test_models = vec![
        "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
        "zai-org/GLM-4.6".to_string(),
        "nearai/gpt-oss-120b".to_string(),
        "dphn/Dolphin-Mistral-24B-Venice-Edition".to_string(),
        "deepseek-ai/DeepSeek-V3.1".to_string(),
    ];

    let providers: Vec<(
        String,
        Arc<dyn inference_providers::InferenceProvider + Send + Sync>,
    )> = test_models
        .into_iter()
        .map(|model_id| (model_id, mock_provider_trait.clone()))
        .collect();

    pool.register_providers(providers).await;

    tracing::info!("Initialized inference provider pool with MockProvider for testing");

    (pool, mock_provider)
}

pub fn is_origin_allowed(origin_str: &str, cors_config: &config::CorsConfig) -> bool {
    if cors_config.exact_matches.iter().any(|o| o == origin_str) {
        return true;
    }

    if let Some(remainder) = origin_str.strip_prefix("http://localhost") {
        if remainder.is_empty() || remainder.starts_with(':') {
            return true;
        }
    }

    if let Some(remainder) = origin_str.strip_prefix("http://127.0.0.1") {
        if remainder.is_empty() || remainder.starts_with(':') {
            return true;
        }
    }

    if origin_str.starts_with("https://")
        && cors_config
            .wildcard_suffixes
            .iter()
            .any(|suffix| origin_str.ends_with(suffix))
    {
        return true;
    }

    false
}

/// Build the complete application router with config
pub fn build_app_with_config(
    database: Arc<Database>,
    auth_components: AuthComponents,
    domain_services: DomainServices,
    config: Arc<ApiConfig>,
) -> Router {
    // Create app state for completions and management routes
    let app_state = AppState {
        organization_service: domain_services.organization_service.clone(),
        workspace_service: domain_services.workspace_service.clone(),
        mcp_manager: domain_services.mcp_manager.clone(),
        completion_service: domain_services.completion_service.clone(),
        models_service: domain_services.models_service.clone(),
        auth_service: auth_components.auth_service.clone(),
        attestation_service: domain_services.attestation_service.clone(),
        usage_service: domain_services.usage_service.clone(),
        user_service: domain_services.user_service.clone(),
        files_service: domain_services.files_service.clone(),
        inference_provider_pool: domain_services.inference_provider_pool.clone(),
        config: config.clone(),
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

    let rate_limit_state = middleware::RateLimitState::default();

    // Build individual route groups
    let auth_routes = build_auth_routes(
        Arc::new(auth_components.clone()),
        &auth_components.auth_state_middleware,
        config.clone(),
    );

    let completion_routes = build_completion_routes(
        app_state.clone(),
        &auth_components.auth_state_middleware,
        usage_state.clone(),
        rate_limit_state.clone(),
    );

    let response_routes = build_response_routes(
        domain_services.response_service,
        domain_services.attestation_service.clone(),
        &auth_components.auth_state_middleware,
        usage_state,
        rate_limit_state.clone(),
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

    let admin_routes = build_admin_routes(
        database.clone(),
        &auth_components.auth_state_middleware,
        config.clone(),
    );

    let invitation_routes =
        build_invitation_routes(app_state.clone(), &auth_components.auth_state_middleware);

    let auth_vpc_routes = build_auth_vpc_routes(app_state.clone());

    let files_routes =
        build_files_routes(app_state.clone(), &auth_components.auth_state_middleware);

    let billing_routes = build_billing_routes(
        domain_services.usage_service.clone(),
        &auth_components.auth_state_middleware,
    );

    // Build OpenAPI and documentation routes
    let openapi_routes = build_openapi_routes();

    // Build health check route (public, no auth required)
    let health_routes = Router::new().route("/health", get(health_check));

    // Create metrics state for HTTP metrics middleware
    let metrics_state = middleware::MetricsState {
        metrics_service: domain_services.metrics_service.clone(),
    };

    // Create CORS layer
    let cors_config = config.cors.clone();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(
            move |origin: &HeaderValue, _request_parts: &axum::http::request::Parts| {
                let origin_str = match origin.to_str() {
                    Ok(s) => s,
                    Err(_) => return false,
                };
                is_origin_allowed(origin_str, &cors_config)
            },
        ))
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

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
                .merge(invitation_routes)
                .merge(auth_vpc_routes)
                .merge(files_routes)
                .merge(billing_routes)
                .merge(health_routes),
        )
        .merge(openapi_routes)
        .layer(cors)
        // Add HTTP metrics middleware to track all requests
        .layer(from_fn_with_state(
            metrics_state,
            middleware::http_metrics_middleware,
        ))
}

/// Build VPC authentication routes
pub fn build_auth_vpc_routes(app_state: AppState) -> Router {
    use crate::routes::auth_vpc::vpc_login;
    use axum::routing::post;

    Router::new()
        .route("/auth/vpc/login", post(vpc_login))
        .with_state(app_state)
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
    auth_components: Arc<AuthComponents>,
    auth_state_middleware: &middleware::AuthState,
    config: Arc<ApiConfig>,
) -> Router {
    use routes::auth::{AuthState, NearAuthState};

    let auth_state: AuthState = (
        auth_components.oauth_manager.clone(),
        auth_components.state_store.clone(),
        auth_components.auth_service.clone(),
        config.clone(),
    );

    let near_auth_state: NearAuthState = (auth_components.near_auth_service.clone(), config);

    // Create a sub-router for the NEAR route with its own state
    let near_router = Router::new()
        .route("/near", post(routes::auth::near_login))
        .with_state(near_auth_state);

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
        .merge(near_router)
        .with_state(auth_state)
}

/// Build completion routes with auth and usage tracking
pub fn build_completion_routes(
    app_state: AppState,
    auth_state_middleware: &AuthState,
    usage_state: middleware::UsageState,
    rate_limit_state: middleware::RateLimitState,
) -> Router {
    let inference_routes = Router::new()
        .route("/chat/completions", post(chat_completions))
        .with_state(app_state.clone())
        .layer(from_fn_with_state(
            usage_state,
            middleware::usage_check_middleware,
        ))
        .layer(from_fn_with_state(
            rate_limit_state.clone(),
            middleware::api_key_rate_limit_middleware,
        ))
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            middleware::auth::auth_middleware_with_workspace_context,
        ))
        .layer(from_fn(middleware::body_hash_middleware));

    let metadata_routes = Router::new()
        .route("/models", get(models))
        .with_state(app_state)
        .layer(from_fn_with_state(
            rate_limit_state.clone(),
            middleware::api_key_rate_limit_middleware,
        ))
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            auth_middleware_with_api_key,
        ));

    Router::new().merge(inference_routes).merge(metadata_routes)
}

/// Build response routes with auth
pub fn build_response_routes(
    response_service: Arc<services::ResponseService>,
    attestation_service: Arc<dyn services::attestation::ports::AttestationServiceTrait>,
    auth_state_middleware: &AuthState,
    usage_state: middleware::UsageState,
    rate_limit_state: middleware::RateLimitState,
) -> Router {
    let route_state = responses::ResponseRouteState {
        response_service: response_service.clone(),
        attestation_service: attestation_service.clone(),
    };

    let inference_routes = Router::new()
        .route("/responses", post(responses::create_response))
        .with_state(route_state.clone())
        .layer(from_fn_with_state(
            usage_state,
            middleware::usage_check_middleware,
        ))
        .layer(from_fn_with_state(
            rate_limit_state.clone(),
            middleware::api_key_rate_limit_middleware,
        ))
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            middleware::auth::auth_middleware_with_workspace_context,
        ))
        .layer(from_fn(middleware::body_hash_middleware));

    let other_routes = Router::new()
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
        .with_state(route_state)
        .layer(from_fn_with_state(
            rate_limit_state.clone(),
            middleware::api_key_rate_limit_middleware,
        ))
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            middleware::auth::auth_middleware_with_workspace_context,
        ));

    Router::new().merge(inference_routes).merge(other_routes)
}

/// Build conversation routes with auth
pub fn build_conversation_routes(
    conversation_service: Arc<services::ConversationService>,
    auth_state_middleware: &AuthState,
) -> Router {
    Router::new()
        .route("/conversations", post(conversations::create_conversation))
        .route(
            "/conversations/batch",
            post(conversations::batch_get_conversations),
        )
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
            "/conversations/{conversation_id}/pin",
            post(conversations::pin_conversation).delete(conversations::unpin_conversation),
        )
        .route(
            "/conversations/{conversation_id}/archive",
            post(conversations::archive_conversation).delete(conversations::unarchive_conversation),
        )
        .route(
            "/conversations/{conversation_id}/clone",
            post(conversations::clone_conversation),
        )
        .route(
            "/conversations/{conversation_id}/items",
            get(conversations::list_conversation_items),
        )
        .route(
            "/conversations/{conversation_id}/items",
            post(conversations::create_conversation_items),
        )
        .with_state(
            conversation_service
                as Arc<dyn services::conversations::ports::ConversationServiceTrait>,
        )
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            auth_middleware_with_api_key,
        ))
}

/// Build attestation routes with auth
pub fn build_attestation_routes(app_state: AppState, auth_state_middleware: &AuthState) -> Router {
    let authenticated_routes = Router::new()
        .route("/signature/{chat_id}", get(get_signature))
        .with_state(app_state.clone())
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            auth_middleware_with_api_key,
        ));

    let public_routes = Router::new()
        .route("/attestation/report", get(get_attestation_report))
        .with_state(app_state);

    Router::new()
        .merge(authenticated_routes)
        .merge(public_routes)
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

/// Build file upload routes
pub fn build_files_routes(app_state: AppState, auth_state_middleware: &AuthState) -> Router {
    use crate::routes::files::MAX_FILE_SIZE;
    use crate::routes::files::*;
    Router::new()
        .route("/files", post(upload_file).get(list_files))
        .route("/files/{file_id}", get(get_file).delete(delete_file))
        .route("/files/{file_id}/content", get(get_file_content))
        .layer(DefaultBodyLimit::max(MAX_FILE_SIZE))
        .with_state(app_state)
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            auth_middleware_with_api_key,
        ))
}

/// Build billing routes with API key auth (HuggingFace billing integration)
pub fn build_billing_routes(
    usage_service: Arc<dyn services::usage::UsageServiceTrait + Send + Sync>,
    auth_state_middleware: &AuthState,
) -> Router {
    let billing_state = BillingRouteState { usage_service };

    Router::new()
        .route("/billing/costs", post(get_billing_costs))
        .with_state(billing_state)
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            middleware::auth::auth_middleware_with_workspace_context,
        ))
}

pub fn build_model_routes(models_service: Arc<dyn ModelsServiceTrait>) -> Router {
    let models_app_state = ModelsAppState { models_service };

    Router::new()
        // Public endpoints - no auth required
        .route("/model/list", get(list_models))
        .route("/model/{model_name}", get(get_model_by_name))
        .with_state(models_app_state)
}

/// Build admin routes (authenticated endpoints)
pub fn build_admin_routes(
    database: Arc<Database>,
    auth_state_middleware: &AuthState,
    config: Arc<ApiConfig>,
) -> Router {
    use crate::middleware::admin_middleware;
    use crate::routes::admin::{
        batch_upsert_models, create_admin_access_token, delete_admin_access_token, delete_model,
        get_model_history, get_organization_limits_history, get_organization_metrics,
        get_organization_timeseries, get_platform_metrics, list_admin_access_tokens,
        list_models as admin_list_models, list_users, update_organization_limits, AdminAppState,
    };
    use database::repositories::{
        AdminAccessTokenRepository, AdminCompositeRepository, PgAnalyticsRepository,
    };
    use services::admin::{AdminServiceImpl, AnalyticsService};

    // Create composite admin repository (handles models, organization limits, and users)
    let admin_repository = Arc::new(AdminCompositeRepository::new(database.pool().clone()));

    // Create admin access token repository
    let admin_access_token_repository =
        Arc::new(AdminAccessTokenRepository::new(database.pool().clone()));

    // Create analytics repository and service
    let analytics_repository = Arc::new(PgAnalyticsRepository::new(database.pool().clone()));
    let analytics_service = Arc::new(AnalyticsService::new(
        analytics_repository as Arc<dyn services::admin::AnalyticsRepository>,
    ));

    // Create admin service with composite repository
    let admin_service = Arc::new(AdminServiceImpl::new(
        admin_repository as Arc<dyn services::admin::AdminRepository>,
    )) as Arc<dyn services::admin::AdminService + Send + Sync>;

    let admin_app_state = AdminAppState {
        admin_service,
        analytics_service,
        auth_service: auth_state_middleware.auth_service.clone(),
        config,
        admin_access_token_repository,
    };

    Router::new()
        .route(
            "/admin/models",
            axum::routing::get(admin_list_models).patch(batch_upsert_models),
        )
        .route(
            "/admin/models/{model_name}",
            axum::routing::delete(delete_model),
        )
        .route(
            "/admin/models/{model_name}/history",
            axum::routing::get(get_model_history),
        )
        .route(
            "/admin/organizations/{org_id}/limits",
            axum::routing::patch(update_organization_limits),
        )
        .route(
            "/admin/organizations/{organization_id}/limits/history",
            axum::routing::get(get_organization_limits_history),
        )
        .route(
            "/admin/organizations/{org_id}/metrics",
            axum::routing::get(get_organization_metrics),
        )
        .route(
            "/admin/organizations/{org_id}/metrics/timeseries",
            axum::routing::get(get_organization_timeseries),
        )
        .route(
            "/admin/platform/metrics",
            axum::routing::get(get_platform_metrics),
        )
        .route("/admin/users", axum::routing::get(list_users))
        .route(
            "/admin/access-tokens",
            axum::routing::post(create_admin_access_token),
        )
        .route(
            "/admin/access-tokens",
            axum::routing::get(list_admin_access_tokens),
        )
        .route(
            "/admin/access-tokens/{token_id}",
            axum::routing::delete(delete_admin_access_token),
        )
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

/// Serve Scalar API Documentation UI
async fn swagger_ui_handler() -> Html<String> {
    Html(
        r#"<!doctype html>
<html lang="en">
<head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>NEAR AI Cloud API Documentation</title>
    <style>
        body {
            margin: 0;
            padding: 0;
        }
    </style>
</head>
<body>
    <script
        id="api-reference"
        type="application/json"
        data-url="/api-docs/openapi.json">
    </script>
    <script>
        var configuration = {
            theme: 'default',
            layout: 'modern',
            defaultHttpClient: {
                targetKey: 'javascript',
                clientKey: 'fetch'
            },
            customCss: `
                --scalar-color-accent: #00C08B;
                --scalar-color-1: #00C08B;
            `,
            searchHotKey: 'k',
            tagsSorter: 'as-is'
        }
    </script>
    <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
</body>
</html>"#
            .to_string(),
    )
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
        assert!(components.security_schemes.contains_key("refresh_token"));
        assert!(components.security_schemes.contains_key("api_key"));

        // Verify servers are not hardcoded (will be set dynamically on client)
        assert!(spec.servers.is_none() || spec.servers.as_ref().unwrap().is_empty());
    }

    /// Example of how to set up the application for E2E testing
    #[tokio::test]
    #[ignore] // Remove ignore to run with a real database and Patroni cluster
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
                inference_timeout: 30 * 60, // 30 minutes
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
                encoding_key: "mock_encoding_key".to_string(),
                github: None,
                google: None,
                near: config::NearConfig::default(),
                admin_domains: vec![],
            },
            database: config::DatabaseConfig {
                primary_app_id: "postgres-patroni-1".to_string(),
                gateway_subdomain: "cvm1.near.ai".to_string(),
                host: None,
                port: 5432,
                database: "test_db".to_string(),
                username: "test_user".to_string(),
                password: "test_pass".to_string(),
                max_connections: 5,
                tls_enabled: false,
                tls_ca_cert_path: None,
                refresh_interval: 30,
                mock: false,
            },
            s3: config::S3Config {
                mock: true,
                bucket: "test-bucket".to_string(),
                region: "us-east-1".to_string(),
                encryption_key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_string(), // Mock 256-bit hex key
            },
            otlp: config::OtlpConfig {
                endpoint: "http://localhost:4317".to_string(),
                protocol: "grpc".to_string(),
            },
            cors: config::CorsConfig::default(),
        };

        // Initialize services
        let database = init_database(&config.database).await;
        let auth_components = init_auth_services(database.clone(), &config);
        let metrics_service = Arc::new(services::metrics::MockMetricsService)
            as Arc<dyn services::metrics::MetricsServiceTrait>;
        let domain_services = init_domain_services(
            database.clone(),
            &config,
            auth_components.organization_service.clone(),
            metrics_service,
        )
        .await;

        // Build the application
        let _app =
            build_app_with_config(database, auth_components, domain_services, Arc::new(config));

        // You can now use `app` with a test server like:
        // let server = axum_test::TestServer::new(app).unwrap();
        // let response = server.get("/v1/models").await;
        // assert_eq!(response.status(), 200);
    }

    /// Example of testing with custom database configuration
    #[tokio::test]
    #[ignore] // Remove ignore when you have a test database with Patroni cluster
    async fn test_with_custom_database() {
        // Create custom database config for testing
        let db_config = config::DatabaseConfig {
            primary_app_id: "postgres-patroni-1".to_string(),
            gateway_subdomain: "cvm1.near.ai".to_string(),
            port: 5432,
            host: None,
            database: "test_db".to_string(),
            username: "test_user".to_string(),
            password: "test_pass".to_string(),
            max_connections: 5,
            tls_enabled: false,
            tls_ca_cert_path: None,
            refresh_interval: 30,
            mock: false,
        };

        // Initialize database with custom config
        let database = init_database(&db_config).await;

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
                inference_timeout: 30 * 60, // 30 minutes
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
                encoding_key: "mock_encoding_key".to_string(),
                github: None,
                google: None,
                near: config::NearConfig::default(),
                admin_domains: vec![],
            },
            database: config::DatabaseConfig {
                primary_app_id: "postgres-patroni-1".to_string(),
                gateway_subdomain: "cvm1.near.ai".to_string(),
                host: None,
                port: 5432,
                database: "test_db".to_string(),
                username: "test_user".to_string(),
                password: "test_pass".to_string(),
                max_connections: 5,
                tls_enabled: false,
                tls_ca_cert_path: None,
                refresh_interval: 30,
                mock: false,
            },
            s3: config::S3Config {
                mock: true,
                bucket: "test-bucket".to_string(),
                region: "us-east-1".to_string(),
                encryption_key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_string(), // Mock 256-bit hex key
            },
            otlp: config::OtlpConfig {
                endpoint: "http://localhost:4317".to_string(),
                protocol: "grpc".to_string(),
            },
            cors: config::CorsConfig::default(),
        };

        let auth_components = init_auth_services(database.clone(), &config);
        let metrics_service = Arc::new(services::metrics::MockMetricsService)
            as Arc<dyn services::metrics::MetricsServiceTrait>;
        let domain_services = init_domain_services(
            database.clone(),
            &config,
            auth_components.organization_service.clone(),
            metrics_service,
        )
        .await;

        let _app =
            build_app_with_config(database, auth_components, domain_services, Arc::new(config));

        // Test the app...
    }

    fn test_cors_config() -> config::CorsConfig {
        config::CorsConfig {
            exact_matches: vec![
                "https://example.com".to_string(),
                "http://test.com".to_string(),
            ],
            wildcard_suffixes: vec![".near.ai".to_string(), "-example.com".to_string()],
        }
    }

    #[test]
    fn test_cors_exact_match_allowed() {
        let config = test_cors_config();
        assert!(is_origin_allowed("https://example.com", &config));
        assert!(is_origin_allowed("http://test.com", &config));
    }

    #[test]
    fn test_cors_exact_match_denied() {
        let config = test_cors_config();
        assert!(!is_origin_allowed("https://evil.com", &config));
        assert!(!is_origin_allowed("http://example.com", &config));
    }

    #[test]
    fn test_cors_localhost_allowed() {
        let config = test_cors_config();
        assert!(is_origin_allowed("http://localhost:3000", &config));
        assert!(is_origin_allowed("http://localhost:8080", &config));
        assert!(is_origin_allowed("http://localhost", &config));
    }

    #[test]
    fn test_cors_localhost_subdomain_denied() {
        let config = test_cors_config();
        assert!(!is_origin_allowed("http://localhost.evil.com", &config));
        assert!(!is_origin_allowed(
            "http://localhost.evil.com:3000",
            &config
        ));
    }

    #[test]
    fn test_cors_127_0_0_1_allowed() {
        let config = test_cors_config();
        assert!(is_origin_allowed("http://127.0.0.1:3000", &config));
        assert!(is_origin_allowed("http://127.0.0.1:8080", &config));
        assert!(is_origin_allowed("http://127.0.0.1", &config));
    }

    #[test]
    fn test_cors_127_0_0_1_subdomain_denied() {
        let config = test_cors_config();
        assert!(!is_origin_allowed("http://127.0.0.1.evil.com", &config));
    }

    #[test]
    fn test_cors_https_wildcard_allowed() {
        let config = test_cors_config();
        assert!(is_origin_allowed("https://app.near.ai", &config));
        assert!(is_origin_allowed("https://chat.near.ai", &config));
        assert!(is_origin_allowed("https://preview-example.com", &config));
    }

    #[test]
    fn test_cors_https_wildcard_denied() {
        let config = test_cors_config();
        assert!(!is_origin_allowed("http://app.near.ai", &config));
        assert!(!is_origin_allowed("https://fakenear.ai", &config));
        assert!(!is_origin_allowed("https://near.ai.evil.com", &config));
    }

    #[test]
    fn test_cors_wildcard_suffix_protection() {
        let config = config::CorsConfig {
            exact_matches: vec![],
            wildcard_suffixes: vec![".near.ai".to_string()],
        };
        assert!(is_origin_allowed("https://app.near.ai", &config));
        assert!(!is_origin_allowed("https://fakenear.ai", &config));
    }

    #[test]
    fn test_cors_wildcard_with_hyphen_allowed() {
        let config = test_cors_config();
        assert!(is_origin_allowed("https://preview-example.com", &config));
        assert!(is_origin_allowed("https://staging-example.com", &config));
    }
}
