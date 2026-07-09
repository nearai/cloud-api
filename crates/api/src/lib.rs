pub mod consts;
pub mod conversions;
pub mod middleware;
pub mod models;
pub mod ohttp_gateway;
pub mod openapi;
pub mod routes;

use crate::ohttp_gateway::{OhttpAttestation, OhttpGateway};
use crate::routes::mcp_server::{handle_mcp_request, McpRouteState};
use crate::routes::ohttp::{ohttp_config, ohttp_relay};
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
        completions::{
            audio_transcriptions, chat_completions, completions, embeddings, image_edits,
            image_generations, models, privacy_classify, privacy_redact, rerank, score,
        },
        conversations,
        feature_requests::{
            list_admin_feature_requests, submit_feature_request, FeatureRequestsRouteState,
        },
        health::health_check,
        models::{get_model_by_name, list_models, ModelsAppState},
        responses,
    },
};
use axum::extract::{Request, State};
use axum::http::{header::CACHE_CONTROL, HeaderValue};
use axum::response::Response;
use axum::{
    extract::DefaultBodyLimit,
    middleware::{from_fn, from_fn_with_state, Next},
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
    web_search::WebSearchService,
};
use std::sync::Arc;
use tower_http::{
    compression::CompressionLayer,
    cors::{AllowOrigin, Any, CorsLayer},
};
use utoipa::OpenApi;

// Audio transcription file size limit (25 MB for OpenAI Whisper API compatibility)
const AUDIO_TRANSCRIPTION_MAX_BODY_SIZE: usize = 25 * 1024 * 1024; // 25 MB

// Privacy classify input is text only, model context is small (e.g. 512 tokens).
// Cap at 256 KB so the route doesn't inherit the 25 MB audio-transcription limit.
const PRIVACY_CLASSIFY_MAX_BODY_SIZE: usize = 256 * 1024; // 256 KB

// OHTTP outer body is the HPKE-encrypted inner BHTTP request. Set to 32 MB to
// cover audio-transcription payloads (≤25 MB) plus HPKE overhead, while
// bounding unauthenticated memory use before the inner request is decrypted.
const OHTTP_MAX_BODY_SIZE: usize = 32 * 1024 * 1024; // 32 MB

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
    pub staking_farm_service: Arc<services::staking_farm::StakingFarmService>,
    pub web_search_provider: Arc<dyn services::responses::tools::WebSearchProviderTrait>,
    pub service_usage_service:
        Arc<dyn services::service_usage::ServiceUsageServiceTrait + Send + Sync>,
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
    let email_sender = services::email::sender_from_config(&config.invitation_email)
        .expect("Failed to initialize invitation email sender");
    let invitations_url = config.invitation_email.invitations_url();

    // Create organization service early (needed by AuthService)
    let organization_service = Arc::new(
        services::organization::OrganizationServiceImpl::new_with_email_sender(
            organization_repo.clone()
                as Arc<dyn services::organization::ports::OrganizationRepository>,
            user_repository.clone(),
            invitation_repo,
            email_sender,
            invitations_url,
        ),
    )
        as Arc<dyn services::organization::OrganizationServiceTrait + Send + Sync>;

    let auth_service: Arc<dyn AuthServiceTrait> = if config.auth.mock {
        // TODO: fix this, it should not use the database pool
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
    let inference_provider_pool = init_inference_providers(database.clone(), config).await;
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
    // Give the provider pool the metrics sink so it can emit the per-tier /
    // fallback counter (cloud_api.provider.requests) from the one layer that
    // knows which trust tier served each request.
    inference_provider_pool.set_metrics_service(metrics_service.clone());

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

    // Note: inference_url models and external providers are loaded in init_inference_providers.
    // Periodic refresh is also started there.

    // Create conversation service
    let conversation_service = Arc::new(services::ConversationService::new(
        conversation_repo.clone(),
        response_repo.clone(),
        response_items_repo.clone(),
    ));

    // Prepare usage repository for attestation service (needed to check stop_reason for disconnected streams)
    let usage_repository_for_attestation = Arc::new(
        database::repositories::OrganizationUsageRepository::new(database.pool().clone()),
    ) as Arc<dyn services::usage::UsageRepository>;

    // Create attestation service
    let attestation_service = Arc::new(
        services::attestation::AttestationService::init(
            attestation_repo,
            inference_provider_pool.clone(),
            models_repo.clone(),
            metrics_service.clone(),
            usage_repository_for_attestation,
        )
        .await
        .unwrap(),
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

    // Create organization limit repository for completion service rate limiting
    let org_limit_repository = Arc::new(database::repositories::PgOrganizationRepository::new(
        database.pool().clone(),
    ))
        as Arc<dyn services::completions::ports::OrganizationConcurrentLimitRepository>;

    // Create completion service with usage tracking (needs usage_service)
    let completion_service = Arc::new(services::CompletionServiceImpl::new(
        inference_provider_pool.clone(),
        attestation_service.clone(),
        usage_service.clone(),
        metrics_service.clone(),
        models_repo.clone() as Arc<dyn services::models::ModelsRepository>,
        org_limit_repository,
    ));

    let brave_search_provider =
        Arc::new(services::responses::tools::brave::BraveWebSearchProvider::new());
    let web_search_provider: Arc<dyn services::responses::tools::WebSearchProviderTrait> =
        brave_search_provider.clone();
    let web_context_search_provider: Arc<
        dyn services::responses::tools::WebContextSearchProviderTrait,
    > = brave_search_provider;

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
        Some(web_search_provider.clone()), // web_search_provider
        Some(web_context_search_provider), // web_context_search_provider
        None,                              // file_search_provider
        files_service.clone(),             // file_service
        organization_service.clone(),
    ));

    let service_repo = Arc::new(database::repositories::ServiceRepository::new(
        database.pool().clone(),
    ));
    let org_service_usage_repo = Arc::new(
        database::repositories::OrganizationServiceUsageRepository::new(database.pool().clone()),
    );
    let service_usage_repo = Arc::new(database::repositories::ServiceUsageRepositoryImpl::new(
        service_repo,
        org_service_usage_repo,
    ))
        as Arc<dyn services::service_usage::ports::ServiceUsageRepositoryTrait>;
    let service_usage_service = Arc::new(services::service_usage::ServiceUsageService::new(
        service_usage_repo,
    ));

    let staking_farm_repository = Arc::new(
        database::repositories::OrganizationStakingFarmSourcesRepository::new(
            database.pool().clone(),
        ),
    ) as Arc<dyn services::staking_farm::StakingFarmRepository>;
    let staking_farm_contract_client = Arc::new(
        services::staking_farm::NearRpcStakingFarmClient::new(
            config.auth.near.rpc_url.clone(),
            config.auth.near.network_id.clone(),
        )
        .expect("Failed to initialize staking farm NEAR RPC client"),
    )
        as Arc<dyn services::staking_farm::StakingFarmContractClient>;
    let staking_farm_service = Arc::new(services::staking_farm::StakingFarmService::new(
        staking_farm_repository,
        staking_farm_contract_client,
        config.staking_farm.clone(),
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
        staking_farm_service,
        web_search_provider,
        service_usage_service,
    }
}

/// Initialize domain services with a custom MCP client factory (for testing)
/// This is a thin wrapper that creates the response service with an injected factory
#[allow(clippy::too_many_arguments)]
pub async fn init_domain_services_with_mcp_factory(
    database: Arc<Database>,
    config: &ApiConfig,
    organization_service: Arc<dyn services::organization::OrganizationServiceTrait + Send + Sync>,
    inference_provider_pool: Arc<services::inference_provider_pool::InferenceProviderPool>,
    metrics_service: Arc<dyn services::metrics::MetricsServiceTrait>,
    mcp_client_factory: Arc<dyn services::responses::tools::McpClientFactory>,
) -> DomainServices {
    // Get the base domain services
    let mut domain_services = init_domain_services_with_pool(
        database.clone(),
        config,
        organization_service.clone(),
        inference_provider_pool.clone(),
        metrics_service.clone(),
    )
    .await;

    // Replace the response service with one that has the MCP factory injected
    let response_repo = Arc::new(database::PgResponseRepository::new(database.pool().clone()));
    let response_items_repo = Arc::new(database::PgResponseItemsRepository::new(
        database.pool().clone(),
    ))
        as Arc<dyn services::responses::ports::ResponseItemRepositoryTrait>;

    let brave_search_provider =
        Arc::new(services::responses::tools::brave::BraveWebSearchProvider::new());
    let web_search_provider: Arc<dyn services::responses::tools::WebSearchProviderTrait> =
        brave_search_provider.clone();
    let web_context_search_provider: Arc<
        dyn services::responses::tools::WebContextSearchProviderTrait,
    > = brave_search_provider;

    let response_service = Arc::new(services::ResponseService::with_mcp_client_factory(
        response_repo,
        response_items_repo,
        inference_provider_pool,
        domain_services.conversation_service.clone(),
        domain_services.completion_service.clone(),
        Some(web_search_provider),
        Some(web_context_search_provider),
        None,
        domain_services.files_service.clone(), // Reuse files_service from base
        organization_service,
        mcp_client_factory,
    ));

    domain_services.response_service = response_service;
    domain_services
}

/// Like `init_domain_services_with_pool` but use the given web search provider (for tests with mock).
/// Rebuilds response_service so both the standalone web search route and Response API (web search
/// tool) use the mock; otherwise response_service would still hold the original Brave provider.
pub async fn init_domain_services_with_pool_and_web_search_provider(
    database: Arc<Database>,
    config: &ApiConfig,
    organization_service: Arc<dyn services::organization::OrganizationServiceTrait + Send + Sync>,
    inference_provider_pool: Arc<services::inference_provider_pool::InferenceProviderPool>,
    metrics_service: Arc<dyn services::metrics::MetricsServiceTrait>,
    web_search_provider: Arc<dyn services::responses::tools::WebSearchProviderTrait>,
) -> DomainServices {
    init_domain_services_with_pool_and_search_providers(
        database,
        config,
        organization_service,
        inference_provider_pool,
        metrics_service,
        web_search_provider,
        None,
    )
    .await
}

/// Like `init_domain_services_with_pool_and_web_search_provider`, but also lets tests
/// inject a Responses-only context-search provider.
pub async fn init_domain_services_with_pool_and_search_providers(
    database: Arc<Database>,
    config: &ApiConfig,
    organization_service: Arc<dyn services::organization::OrganizationServiceTrait + Send + Sync>,
    inference_provider_pool: Arc<services::inference_provider_pool::InferenceProviderPool>,
    metrics_service: Arc<dyn services::metrics::MetricsServiceTrait>,
    web_search_provider: Arc<dyn services::responses::tools::WebSearchProviderTrait>,
    web_context_search_provider: Option<
        Arc<dyn services::responses::tools::WebContextSearchProviderTrait>,
    >,
) -> DomainServices {
    let mut domain_services = init_domain_services_with_pool(
        database.clone(),
        config,
        organization_service.clone(),
        inference_provider_pool.clone(),
        metrics_service,
    )
    .await;

    let response_repo = Arc::new(database::PgResponseRepository::new(database.pool().clone()));
    let response_items_repo = Arc::new(database::PgResponseItemsRepository::new(
        database.pool().clone(),
    ))
        as Arc<dyn services::responses::ports::ResponseItemRepositoryTrait>;

    let response_service = Arc::new(services::ResponseService::new(
        response_repo,
        response_items_repo,
        inference_provider_pool,
        domain_services.conversation_service.clone(),
        domain_services.completion_service.clone(),
        Some(web_search_provider.clone()),
        web_context_search_provider,
        None,
        domain_services.files_service.clone(),
        organization_service,
    ));

    domain_services.web_search_provider = web_search_provider;
    domain_services.response_service = response_service;
    domain_services
}

/// Standard OpenAI sampling knobs Chutes (sglang) honors, expressed in
/// OpenRouter's fixed `supported_sampling_parameters` vocabulary. Seeded onto
/// every auto-created Chutes catalog row so `GET /v1/models` advertises real
/// capabilities instead of an empty list (which silently disables routing for
/// OpenRouter-style consumers). `n` is intentionally omitted: it is not part of
/// OpenRouter's vocabulary. Must remain a subset of `routes::admin::VALID_SAMPLING_PARAMS`.
pub(crate) const CHUTES_SUPPORTED_SAMPLING_PARAMS: &[&str] = &[
    "temperature",
    "top_p",
    "frequency_penalty",
    "presence_penalty",
    "stop",
    "seed",
    "max_tokens",
];

/// Feature capabilities Chutes (sglang) exposes, in OpenRouter's fixed
/// `supported_features` vocabulary: `tools` => tool/function-calling, `json_mode`
/// => JSON `response_format`. Streaming is always supported but is not a member
/// of OpenRouter's feature vocabulary, so it is not advertised here. Must remain
/// a subset of `routes::admin::VALID_FEATURES`.
///
/// `tools` is a *default* assumption, not a universal guarantee: tool-calling in
/// sglang is model-family specific (needs a compatible chat template + tool-call
/// parser), so a family without it would be over-advertised here — the inverse of
/// the empty-array bug. That risk is bounded because the seed lands INACTIVE: an
/// operator must PATCH the row (and is warned to verify tool support, clearing
/// `supported_features` if absent) before any traffic is served.
pub(crate) const CHUTES_SUPPORTED_FEATURES: &[&str] = &["tools", "json_mode"];

/// Ensure a Chutes (attested) model has a catalog row in the `models` table.
///
/// The data plane rejects any model without an active `models` row *before*
/// reaching the provider pool (`resolve_and_get_model` in completions), so a
/// pinned Chutes provider registered purely in-memory would 404 every request
/// even with `ENABLE_CHUTES=true` and a valid key. Worse, usage rows carry a
/// `FOREIGN KEY (model_id) REFERENCES models(id)` — a synthesized id can't be
/// billed — so the row must genuinely exist. Seed it here at startup.
///
/// Idempotent and non-clobbering: if an active row already exists (operator
/// pre-seeded it with real pricing/metadata via the admin API) we leave it
/// untouched. We only INSERT when missing, with attestation flags set and
/// **zero pricing** — the operator must set real per-token rates via
/// `PATCH /v1/admin/models` before serving paid traffic, which we warn about.
async fn ensure_chutes_catalog_row(
    models_repo: &database::repositories::ModelRepository,
    model_name: &str,
) {
    // Use the *unfiltered* lookup (not get_active_model_by_name): a deliberately
    // disabled row (is_active=false) must be respected, not silently re-activated
    // and clobbered by the seed path below.
    match models_repo.get_by_internal_name(model_name).await {
        Ok(Some(existing)) => {
            // Already in the catalog — respect operator configuration verbatim.
            // Surface a warning if the metadata contradicts attested serving so
            // a misconfigured row (e.g. attestation_supported=false) is visible.
            if !existing.is_active {
                tracing::warn!(
                    model = %model_name,
                    "Chutes model has a DISABLED catalog row (is_active=false); requests will \
                     404 by design — re-enable via PATCH /v1/admin/models if that's unintended"
                );
            } else if !existing.attestation_supported {
                tracing::warn!(
                    model = %model_name,
                    "Chutes model has an existing catalog row with attestation_supported=false; \
                     E2EE/signature handling may misbehave — fix via PATCH /v1/admin/models"
                );
            } else if existing.supported_sampling_parameters.is_empty()
                && existing.supported_features.is_empty()
            {
                // This is the exact #781 (M1) bug state on a pre-existing row: both
                // capability arrays are still the empty V0051 default, so
                // `GET /v1/models` advertises the model as supporting *nothing* and
                // OpenRouter-style routers won't route tool calls to it. New rows are
                // seeded non-empty above; existing rows are backfilled by migration
                // V0060. Warn in case a row predates the migration or was cleared.
                tracing::warn!(
                    model = %model_name,
                    "Chutes model has an existing catalog row with EMPTY supported_features \
                     and supported_sampling_parameters — OpenRouter-style routers will refuse \
                     to route tool calls to it; backfilled by migration V0060, or set via \
                     PATCH /v1/admin/models"
                );
            } else {
                tracing::info!(model = %model_name, "Chutes model already in catalog");
            }
        }
        Ok(None) => {
            // Friendly display name = last path segment; owner = leading segment.
            let display_name = model_name.rsplit('/').next().unwrap_or(model_name);
            let owned_by = model_name.split('/').next().unwrap_or("chutes");
            let req = database::models::UpdateModelPricingRequest {
                model_display_name: Some(display_name.to_string()),
                model_description: Some(
                    "Attested model served via Chutes TEE (verified end-to-end by NEAR AI)."
                        .to_string(),
                ),
                // Generous default; operator should set the model's real context
                // window via the admin API. Not enforced as a hard reject here.
                context_length: Some(128_000),
                verifiable: Some(true),
                attestation_supported: Some(true),
                // Seed INACTIVE. `is_active=false` is the only field that actually
                // gates serving (`resolve_and_get_model` filters `WHERE is_active`;
                // `is_ready` is pure display metadata and does NOT gate). Seeding
                // inactive makes it *impossible* to serve — and therefore bill at
                // the zero default pricing — until an operator explicitly sets real
                // per-token rates AND flips is_active=true (one admin PATCH). This
                // closes the unpriced-serving window rather than merely warning.
                is_active: Some(false),
                is_ready: Some(Some(false)),
                provider_type: Some("chutes".to_string()),
                owned_by: Some(owned_by.to_string()),
                input_modalities: Some(vec!["text".to_string()]),
                output_modalities: Some(vec!["text".to_string()]),
                // OpenRouter-style routers gate tool/function-calling on these two
                // arrays; leaving them empty (the SQL default) silently advertises a
                // Chutes model as supporting *nothing*, so routers refuse to route
                // tool calls to it. Chutes serves via sglang on an OpenAI-compatible
                // surface, so seed the standard OpenAI knobs it honors. Operators can
                // still override via PATCH /v1/admin/models. Both lists are restricted
                // to OpenRouter's fixed vocabulary (asserted by a unit test below) so
                // the seeded row would pass the same admin write-path validation.
                supported_sampling_parameters: Some(
                    CHUTES_SUPPORTED_SAMPLING_PARAMS
                        .iter()
                        .copied()
                        .map(String::from)
                        .collect(),
                ),
                supported_features: Some(
                    CHUTES_SUPPORTED_FEATURES
                        .iter()
                        .copied()
                        .map(String::from)
                        .collect(),
                ),
                // Pricing left None -> defaults to 0 on INSERT. The inactive seed
                // above prevents this zero price from ever being charged.
                ..Default::default()
            };
            // INSERT ... ON CONFLICT DO NOTHING: if an operator created/activated
            // the row concurrently with startup, their row wins and is left
            // untouched (no clobbering is_active/pricing back to the seed defaults).
            match models_repo.seed_model_if_absent(model_name, &req).await {
                Ok(Some(_)) => {
                    tracing::warn!(
                        model = %model_name,
                        "Seeded Chutes catalog row as INACTIVE with zero pricing — set real \
                         per-token rates AND is_active=true via PATCH /v1/admin/models to serve \
                         (kept inactive so paid traffic can't be billed at $0). The seed \
                         advertises `tools`/`json_mode`: verify this model family actually \
                         supports tool-calling in sglang (per-family parser + compatible chat \
                         template) before activating, and clear `supported_features` via the \
                         same PATCH if it doesn't"
                    );
                }
                Ok(None) => {
                    tracing::info!(
                        model = %model_name,
                        "Chutes catalog row already present (created concurrently); left untouched"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        model = %model_name, error = %e,
                        "Failed to seed Chutes catalog row; requests for this model will 404 \
                         until a row exists (create it via PATCH /v1/admin/models)"
                    );
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                model = %model_name, error = %e,
                "Could not check catalog for Chutes model; skipping auto-seed"
            );
        }
    }
}

/// Initialize inference provider pool
///
/// Loads inference_url models and external providers from the database,
/// then starts a periodic refresh task to keep them in sync.
pub async fn init_inference_providers(
    database: Arc<Database>,
    config: &ApiConfig,
) -> Arc<services::inference_provider_pool::InferenceProviderPool> {
    let api_key = config.inference_api_key.clone();

    let pool = Arc::new(
        services::inference_provider_pool::InferenceProviderPool::new(
            api_key,
            config.external_providers.clone(),
        ),
    );

    let models_repo = Arc::new(database::repositories::ModelRepository::new(
        database.pool().clone(),
    ));
    let models_source =
        models_repo.clone() as Arc<dyn services::inference_provider_pool::ExternalModelsSource>;

    // Fail-closed reservation (MUST run before external/discovery loads below):
    // when Chutes is enabled, reserve EVERY configured canonical id as a pinned
    // (verifiable) model up front — even before we try to build the providers. This
    // guarantees a plaintext external/OpenRouter row sharing a canonical id can
    // never register for it, even if the Chutes provider fails to build (missing
    // key / construction error). A reserved id then serves only its attested
    // provider(s) or fails closed (404); it can never silently serve plaintext for
    // a model an operator configured as verifiable.
    if config.external_providers.enable_chutes {
        let canonical_ids: Vec<String> = config
            .external_providers
            .chutes_models
            .iter()
            .map(|e| e.canonical_id.clone())
            .collect();
        if !canonical_ids.is_empty() {
            pool.reserve_pinned_models(&canonical_ids);
            tracing::info!(
                count = canonical_ids.len(),
                "Reserved Chutes canonical ids as verifiable (fail-closed) before external load"
            );
        }
    }

    // Load inference_url models (our own vLLM/SGLang backends)
    match models_source.fetch_inference_url_models().await {
        Ok(models) if !models.is_empty() => {
            tracing::info!(count = models.len(), "Loading inference_url models");
            pool.load_inference_url_models(models, false).await;
        }
        Ok(_) => {
            tracing::info!("No inference_url models found in database");
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to fetch inference_url models");
        }
    }

    // Load external providers (OpenAI, Anthropic, Gemini, etc.)
    match models_source.fetch_external_models().await {
        Ok(models) if !models.is_empty() => {
            tracing::info!(count = models.len(), "Loading external providers");
            if let Err(e) = pool.load_external_providers(models).await {
                tracing::warn!(error = %e, "Failed to load some external providers");
            }
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "Failed to fetch external models");
        }
    }

    // Start periodic refresh task
    let refresh_interval = config.external_providers.refresh_interval_secs;
    pool.clone()
        .start_refresh_task(models_source, refresh_interval)
        .await;

    // Chutes attested provider — hard-off by default (`ENABLE_CHUTES`). Each model
    // is served over a verified ML-KEM E2EE channel: every request attests the
    // chosen instance (TDX quote + report_data bindings + register-pinned
    // measurement + GPU) before encapsulating, so an unverified backend can never
    // serve a Chutes response. Registration is gated on the flag + an API key +
    // at least one model id.
    if config.external_providers.enable_chutes {
        match &config.external_providers.chutes_api_key {
            Some(api_key) if !config.external_providers.chutes_models.is_empty() => {
                let pccs_url = config.external_providers.pccs_url.clone();
                let allow_streaming = config.external_providers.chutes_enable_streaming;
                let verifier: Arc<
                    dyn inference_providers::attested::chutes::verifier_port::ChutesInstanceVerifier,
                > = Arc::new(services::attestation::chutes::ChutesBackendVerifier::new(
                    services::attestation::chutes::vetted_golden_measurements(),
                    pccs_url,
                ));
                for entry in &config.external_providers.chutes_models {
                    // The provider talks to Chutes with the chute SLUG (request_body
                    // pins it + cached_chute_id resolves it); we expose/route under
                    // the CANONICAL id (the NEAR-served id when NEAR also serves the
                    // model, else the OpenRouter id) — never the raw `-TEE` slug.
                    let cfg = inference_providers::attested::chutes::Config::new(
                        api_key.clone(),
                        entry.chute_slug.clone(),
                        config.external_providers.timeout_seconds,
                    )
                    .with_canonical_id(entry.canonical_id.clone())
                    .with_streaming(allow_streaming);
                    match inference_providers::attested::chutes::Provider::new(
                        cfg,
                        verifier.clone(),
                    ) {
                        Ok(provider) => {
                            // Ensure a catalog row exists under the canonical id so the
                            // data plane resolves the model (and usage bills against a
                            // real id). If NEAR already serves this id, its row is left
                            // untouched and we just add Chutes as a fallback provider.
                            ensure_chutes_catalog_row(&models_repo, &entry.canonical_id).await;
                            // Pinned SECONDARY: pushed onto the canonical id's provider
                            // list (coexists with NEAR's own providers) and excluded from
                            // discovery's stale-removal/overwrite. Tier ordering puts NEAR
                            // first and Chutes as fallback; a Chutes-only id has just this
                            // provider, so it serves as primary.
                            pool.register_pinned_secondary_provider(
                                entry.canonical_id.clone(),
                                Arc::new(provider),
                                entry.max_context_tokens,
                            )
                            .await;
                            tracing::info!(
                                canonical = %entry.canonical_id,
                                chute_slug = %entry.chute_slug,
                                "Registered Chutes attested provider (fallback tier)"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                canonical = %entry.canonical_id,
                                chute_slug = %entry.chute_slug,
                                error = %e,
                                "Failed to build Chutes provider"
                            );
                        }
                    }
                }
            }
            _ => {
                tracing::warn!(
                    "ENABLE_CHUTES is set but CHUTES_API_KEY or CHUTES_MODELS is missing; \
                     not registering any Chutes provider"
                );
            }
        }
    } else if !config.external_providers.chutes_models.is_empty() {
        // Flag off but models still listed: any *active* catalog row left over
        // from a previous run would resolve to a model with no registered provider
        // (per-request provider errors, not a clean 404). Warn so an operator
        // notices and deactivates those rows (PATCH is_active=false).
        tracing::warn!(
            models = ?config.external_providers.chutes_models,
            "ENABLE_CHUTES is off but CHUTES_MODELS is set; if any of these have an active \
             catalog row, requests will surface provider errors — deactivate them via \
             PATCH /v1/admin/models or re-enable ENABLE_CHUTES"
        );
    }

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

    let pool = Arc::new(
        services::inference_provider_pool::InferenceProviderPool::new(
            None,
            config::ExternalProvidersConfig::default(),
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
        "Qwen/Qwen3-Omni-30B-A3B-Instruct".to_string(),
        "Qwen/Qwen-Image-2512".to_string(),
        "Qwen/Qwen3-Reranker-0.6B".to_string(),
        "Qwen/Qwen3-Embedding-0.6B".to_string(),
        "openai/privacy-filter".to_string(),
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
    // Create analytics service (shared between user and admin routes)
    let analytics_repository = Arc::new(database::repositories::PgAnalyticsRepository::new(
        database.pool().clone(),
    ));
    let analytics_service = Arc::new(services::admin::AnalyticsService::new(
        analytics_repository as Arc<dyn services::admin::AnalyticsRepository>,
    ));

    // Initialize OHTTP gateway (RFC 9458) if OHTTP_ENABLED=true.
    // The gateway key is deterministically derived from the same dstack KMS Ed25519 seed
    // used for chat-completion signing — all instances share the same HPKE public key.
    let (ohttp_gateway, ohttp_attestation) = if config.server.ohttp_enabled {
        let seed = domain_services.attestation_service.ed25519_secret_bytes();
        match OhttpGateway::new(&seed) {
            Ok(gw) => {
                tracing::info!(
                    ohttp_key_config = %hex::encode(gw.config_bytes()),
                    "OHTTP gateway enabled"
                );
                let (signature, signing_key) = domain_services
                    .attestation_service
                    .sign_ohttp_attestation(gw.config_bytes());
                let attestation = OhttpAttestation {
                    signing_algo: "ed25519".to_string(),
                    signing_key,
                    key_config: hex::encode(gw.config_bytes()),
                    signature,
                };
                (Some(Arc::new(gw)), Some(attestation))
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to initialize OHTTP gateway");
                (None, None)
            }
        }
    } else {
        (None, None)
    };

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
        service_usage_service: domain_services.service_usage_service.clone(),
        user_service: domain_services.user_service.clone(),
        files_service: domain_services.files_service.clone(),
        inference_provider_pool: domain_services.inference_provider_pool.clone(),
        metrics_service: domain_services.metrics_service.clone(),
        analytics_service: analytics_service.clone(),
        staking_farm_service: domain_services.staking_farm_service.clone(),
        config: config.clone(),
        ohttp_gateway,
        ohttp_attestation,
        http_client: reqwest::Client::new(),
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
        staking_farm_service: domain_services.staking_farm_service.clone(),
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

    let gateway_routes = build_gateway_routes(
        app_state.clone(),
        &auth_components.auth_state_middleware,
        usage_state.clone(),
        rate_limit_state.clone(),
    );

    let internal_routes = build_internal_routes(app_state.clone());

    let response_routes = build_response_routes(
        domain_services.response_service,
        domain_services.attestation_service.clone(),
        &auth_components.auth_state_middleware,
        usage_state.clone(),
        rate_limit_state.clone(),
    );
    let unsupported_openai_routes = build_unsupported_openai_routes(
        &auth_components.auth_state_middleware,
        rate_limit_state.clone(),
    );

    let mcp_routes = build_mcp_routes(
        domain_services.web_search_provider.clone(),
        domain_services.service_usage_service.clone(),
        &auth_components.auth_state_middleware,
        usage_state.clone(),
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

    let services_routes = build_services_routes(database.pool().clone());

    let admin_routes = build_admin_routes(
        database.clone(),
        &auth_components.auth_state_middleware,
        config.clone(),
        AdminRouteServices {
            inference_provider_pool: app_state.inference_provider_pool.clone(),
            analytics_service,
            staking_farm_service: app_state.staking_farm_service.clone(),
            models_service: domain_services.models_service.clone(),
            completion_service: domain_services.completion_service.clone(),
            organization_service: domain_services.organization_service.clone(),
            usage_service: domain_services.usage_service.clone(),
        },
    );

    let invitation_routes =
        build_invitation_routes(app_state.clone(), &auth_components.auth_state_middleware);

    let auth_vpc_routes = build_auth_vpc_routes(app_state.clone());

    let files_routes =
        build_files_routes(app_state.clone(), &auth_components.auth_state_middleware);

    let feature_request_routes = build_feature_request_routes(
        database.pool().clone(),
        &auth_components.auth_state_middleware,
    );

    let billing_routes = build_billing_routes(
        domain_services.usage_service.clone(),
        &auth_components.auth_state_middleware,
    );

    // Build OpenAPI and documentation routes
    let openapi_routes = build_openapi_routes();

    // Build health check route (public, no auth required).
    // Short cache window: enough to absorb thundering-herd from monitors that
    // hammer /v1/health, but short enough that real outages surface quickly.
    let health_routes = Router::new()
        .route("/health", get(health_check))
        .layer(cache_control_layer("public, max-age=5"));

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

    // OHTTP routes: `POST /ohttp` and `GET /.well-known/ohttp-gateway` are at the
    // root (not under /v1) so clients can reach them without version-prefixing.
    // `GET /v1/ohttp/config` is a convenience alias nested under /v1.
    let ohttp_root_routes = Router::new()
        .route(
            "/ohttp",
            post(ohttp_relay).layer(DefaultBodyLimit::max(OHTTP_MAX_BODY_SIZE)),
        )
        .route("/.well-known/ohttp-gateway", get(ohttp_config))
        .with_state(app_state.clone());

    Router::new()
        .nest(
            "/v1",
            Router::new()
                .nest("/auth", auth_routes)
                .merge(completion_routes)
                .merge(response_routes)
                .merge(unsupported_openai_routes)
                .merge(conversation_routes)
                .merge(management_routes)
                .merge(workspace_routes)
                .merge(attestation_routes.clone())
                .merge(model_routes)
                .merge(services_routes)
                .merge(admin_routes)
                .merge(invitation_routes)
                .merge(auth_vpc_routes)
                .merge(files_routes)
                .merge(feature_request_routes)
                .merge(billing_routes)
                .merge(gateway_routes)
                .merge(internal_routes)
                .merge(health_routes)
                // GET /v1/ohttp/config — convenience alias for the key config endpoint
                .route(
                    "/ohttp/config",
                    get(ohttp_config).with_state(app_state.clone()),
                ),
        )
        .merge(openapi_routes)
        .merge(mcp_routes)
        .merge(ohttp_root_routes)
        .layer(cors)
        // Add HTTP metrics middleware to track all requests
        .layer(from_fn_with_state(
            metrics_state,
            middleware::http_metrics_middleware,
        ))
        // Response compression (gzip + brotli). Applied after metrics so it sees
        // all routes. `CompressionLayer` auto-detects the response Content-Type
        // and skips `text/event-stream` (SSE), so streaming chat completions and
        // /v1/responses remain unaffected. Signed-response payloads (attestation
        // endpoints) sign the *request* body hash, not the HTTP response body,
        // so compression is safe for them as well.
        .layer(CompressionLayer::new())
        .layer(from_fn(middleware::request_correlation_middleware))
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
    use crate::routes::files::MAX_FILE_SIZE;

    // Text-based inference routes (chat/completions, image generation, audio transcription, rerank, score)
    // Use default body limit (~2 MB) since they only accept JSON
    let text_inference_routes = Router::new()
        .route("/chat/completions", post(chat_completions))
        .route("/completions", post(completions))
        .route("/images/generations", post(image_generations))
        .route("/audio/transcriptions", post(audio_transcriptions))
        .route("/rerank", post(rerank))
        .route("/embeddings", post(embeddings))
        .route("/score", post(score))
        // Override the router-level audio limit (25 MB) for privacy/classify: this is a
        // text-only endpoint, so a 256 KB cap is more appropriate.
        .route(
            "/privacy/classify",
            post(privacy_classify).layer(DefaultBodyLimit::max(PRIVACY_CLASSIFY_MAX_BODY_SIZE)),
        )
        // /privacy/redact runs a classify call under the hood, so the same
        // 256 KB cap applies.
        .route(
            "/privacy/redact",
            post(privacy_redact).layer(DefaultBodyLimit::max(PRIVACY_CLASSIFY_MAX_BODY_SIZE)),
        )
        .layer(DefaultBodyLimit::max(AUDIO_TRANSCRIPTION_MAX_BODY_SIZE))
        .with_state(app_state.clone())
        .layer(from_fn_with_state(
            usage_state.clone(),
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

    // File-based inference routes (image edits)
    // Apply 512 MB limit only to endpoints that accept file uploads
    // IMPORTANT: body_hash_middleware is placed AFTER auth to prevent buffering
    // unauthenticated requests. Auth failures prevent memory exhaustion DoS attacks.
    let file_inference_routes = Router::new()
        .route("/images/edits", post(image_edits))
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
        .layer(from_fn(middleware::body_hash_middleware))
        .layer(DefaultBodyLimit::max(MAX_FILE_SIZE));

    let metadata_routes = Router::new()
        .route("/models", get(models))
        .with_state(app_state)
        // Public, OpenAI-compatible model catalog. The response is identical for
        // all clients and changes only when an admin updates the catalog.
        .layer(cache_control_layer(
            "public, max-age=30, stale-while-revalidate=120",
        ));

    Router::new()
        .merge(text_inference_routes)
        .merge(file_inference_routes)
        .merge(metadata_routes)
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

/// Build explicit not-implemented handlers for recognized OpenAI-compatible
/// endpoints that cloud-api does not support yet.
///
/// These routes intentionally sit behind the normal API-key middleware, so
/// unauthenticated callers receive the standard 401 before the 501 placeholder.
pub fn build_unsupported_openai_routes(
    auth_state_middleware: &AuthState,
    rate_limit_state: middleware::RateLimitState,
) -> Router {
    routes::unsupported::openai_compat_routes()
        .layer(from_fn_with_state(
            rate_limit_state,
            middleware::api_key_rate_limit_middleware,
        ))
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            middleware::auth::auth_middleware_with_workspace_context,
        ))
}

pub fn build_mcp_routes(
    web_search_provider: Arc<dyn services::responses::tools::WebSearchProviderTrait>,
    service_usage_service: Arc<dyn services::service_usage::ServiceUsageServiceTrait + Send + Sync>,
    auth_state_middleware: &AuthState,
    usage_state: middleware::UsageState,
    rate_limit_state: middleware::RateLimitState,
) -> Router {
    let route_state = McpRouteState {
        web_search_service: Arc::new(WebSearchService::new(
            web_search_provider,
            service_usage_service,
        )),
        usage_state: usage_state.clone(),
        rate_limit_state: rate_limit_state.clone(),
    };

    Router::new()
        .route("/mcp", post(handle_mcp_request))
        .with_state(route_state)
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            middleware::auth::auth_middleware_with_workspace_context,
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

/// Build feature request routes for user submissions and admin aggregation.
pub fn build_feature_request_routes(
    pool: database::DbPool,
    auth_state_middleware: &AuthState,
) -> Router {
    use crate::middleware::{admin_middleware, auth_middleware};

    let state = FeatureRequestsRouteState {
        repository: Arc::new(database::repositories::FeatureRequestRepository::new(pool)),
    };

    let user_routes = Router::new()
        .route("/feature-requests", post(submit_feature_request))
        .with_state(state.clone())
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            auth_middleware,
        ));

    let admin_routes = Router::new()
        .route("/admin/feature-requests", get(list_admin_feature_requests))
        .with_state(state)
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            admin_middleware,
        ));

    Router::new().merge(user_routes).merge(admin_routes)
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

/// Build gateway routes for external model gateways to validate API keys.
/// Reuses the same auth, rate limiting, and usage check middleware as completions.
pub fn build_gateway_routes(
    app_state: AppState,
    auth_state_middleware: &AuthState,
    usage_state: middleware::UsageState,
    rate_limit_state: middleware::RateLimitState,
) -> Router {
    Router::new()
        .route(
            "/check_api_key",
            post(crate::routes::gateway::check_api_key),
        )
        .with_state(app_state)
        .layer(from_fn_with_state(
            usage_state,
            middleware::usage_check_middleware,
        ))
        .layer(from_fn_with_state(
            rate_limit_state,
            middleware::api_key_rate_limit_middleware,
        ))
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            middleware::auth::auth_middleware_with_workspace_context,
        ))
}

/// Build internal-only routes authenticated by the shared
/// `CLOUD_API_USAGE_TOKEN` service secret rather than the standard `sk-…`
/// API-key middleware. Mounted under `/v1/internal/*` after the global
/// `/v1` nest. Today's only route is `POST /internal/usage` (for
/// inference-proxy's service-token reporter); future internal endpoints
/// can be added here without touching the sk-auth stack.
pub fn build_internal_routes(app_state: AppState) -> Router {
    Router::new()
        .route(
            "/internal/usage",
            post(crate::routes::usage::record_usage_internal),
        )
        .with_state(app_state)
}

pub fn build_model_routes(models_service: Arc<dyn ModelsServiceTrait>) -> Router {
    let models_app_state = ModelsAppState { models_service };

    Router::new()
        // Public endpoints - no auth required
        .route("/model/list", get(list_models))
        .route("/model/{model_name}", get(get_model_by_name))
        .with_state(models_app_state)
        // Public, anonymous, identical-for-all-clients responses that change
        // only when an admin updates the model catalog. 30s fresh window plus
        // 120s stale-while-revalidate lets CDNs/browsers serve cached copies
        // instantly while refreshing in the background.
        .layer(cache_control_layer(
            "public, max-age=30, stale-while-revalidate=120",
        ))
}

/// Build public services routes (no auth) — GET /v1/services, GET /v1/services/{service_name}
pub fn build_services_routes(pool: database::DbPool) -> Router {
    use crate::routes::services::{get_service_by_name, list_services, ServicesRouteState};
    use database::repositories::ServiceRepository;

    let service_repository = Arc::new(ServiceRepository::new(pool));
    let state = ServicesRouteState { service_repository };

    Router::new()
        .route("/services", get(list_services))
        .route("/services/{service_name}", get(get_service_by_name))
        .with_state(state)
        // Same rationale as `/v1/model/*` — public, anonymous, admin-write speed.
        .layer(cache_control_layer(
            "public, max-age=30, stale-while-revalidate=120",
        ))
}

/// Middleware: only insert `Cache-Control` on successful (2xx) responses, and
/// only if the handler did not already set one.
///
/// Setting `Cache-Control` on 4xx/5xx is unsafe: cooperating intermediaries
/// (Cloudflare "Cache Everything", Fastly, browsers) may pin transient errors
/// for the declared TTL. Gating on success ensures a single DB blip on
/// `list_models` doesn't pin a 500 for ~2.5 minutes, and that a temporary
/// "not found" on `get_model_by_name` clears the moment the model is added.
async fn cache_control_on_success(
    State(value): State<HeaderValue>,
    req: Request,
    next: Next,
) -> Response {
    let mut res = next.run(req).await;
    if res.status().is_success() && !res.headers().contains_key(CACHE_CONTROL) {
        res.headers_mut().insert(CACHE_CONTROL, value);
    }
    res
}

// Type aliases for `cache_control_layer`'s return type. They name the
// otherwise-unnameable function-pointer + future combination so the helper's
// signature stays readable (and satisfies clippy::type_complexity).
type CacheControlFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + 'static>>;
type CacheControlShim = fn(State<HeaderValue>, Request, Next) -> CacheControlFuture;
type CacheControlLayer =
    axum::middleware::FromFnLayer<CacheControlShim, HeaderValue, (State<HeaderValue>, Request)>;

/// Build a `Cache-Control`-setting middleware layer for a route group.
///
/// Used only on public, anonymous endpoints whose responses do not vary by
/// user/API-key/session. Do NOT add to authenticated or user-specific routes.
///
/// Only applies the header on success — see [`cache_control_on_success`].
fn cache_control_layer(value: &'static str) -> CacheControlLayer {
    // Coerce the async fn to a function pointer with a fully-nameable type so
    // the helper's return type doesn't leak unnameable opaque generics.
    fn shim(state: State<HeaderValue>, req: Request, next: Next) -> CacheControlFuture {
        Box::pin(cache_control_on_success(state, req, next))
    }
    let f: CacheControlShim = shim;
    from_fn_with_state(HeaderValue::from_static(value), f)
}

/// Build admin routes (authenticated endpoints)
pub struct AdminRouteServices {
    pub inference_provider_pool: Arc<services::inference_provider_pool::InferenceProviderPool>,
    pub analytics_service: Arc<services::admin::AnalyticsService>,
    pub staking_farm_service: Arc<services::staking_farm::StakingFarmService>,
    pub models_service: Arc<services::models::ModelsServiceImpl>,
    pub completion_service: Arc<services::CompletionServiceImpl>,
    pub organization_service:
        Arc<dyn services::organization::OrganizationServiceTrait + Send + Sync>,
    pub usage_service: Arc<dyn services::usage::UsageServiceTrait + Send + Sync>,
}

pub fn build_admin_routes(
    database: Arc<Database>,
    auth_state_middleware: &AuthState,
    config: Arc<ApiConfig>,
    services: AdminRouteServices,
) -> Router {
    use crate::middleware::admin_middleware;
    use crate::routes::admin::{
        batch_upsert_models, cancel_model_pricing_change, confirm_model_deprecation,
        confirm_model_pricing_changes, create_admin_access_token, create_service,
        delete_admin_access_token, delete_model, deprecate_model, get_admin_organization_balance,
        get_billing_summary, get_infra_summary, get_model_consumption_timeseries,
        get_model_history, get_model_revenue, get_org_revenue,
        get_organization as get_admin_organization, get_organization_concurrent_limit,
        get_organization_limits_history, get_organization_metrics, get_organization_timeseries,
        get_performance_timeseries, get_platform_metrics, get_platform_timeseries,
        get_revenue_density, list_admin_access_tokens, list_invitation_email_deliveries,
        list_model_pricing_changes, list_models as admin_list_models, list_organization_members,
        list_organizations, list_users, preview_model_deprecation, preview_model_pricing_changes,
        resend_invitation_email, update_organization_concurrent_limit, update_organization_limits,
        update_service, AdminAppState,
    };
    use crate::routes::staking_farm::{
        get_admin_organization_staking_farm, sync_admin_organization_staking_farm,
    };
    use database::repositories::{AdminAccessTokenRepository, AdminCompositeRepository};
    use services::admin::AdminServiceImpl;

    // Create composite admin repository (handles models, organization limits, and users)
    let admin_repository = Arc::new(AdminCompositeRepository::new(database.pool().clone()));

    // Create admin access token repository
    let admin_access_token_repository =
        Arc::new(AdminAccessTokenRepository::new(database.pool().clone()));

    // Create admin service with composite repository.
    //
    // The admin service holds a reference to the `models_service` so it can
    // invalidate the public `/v1/model/list` cache after admin writes
    // (`upsert`, `delete`, `deprecate`) that mutate the `models` or
    // `model_aliases` tables. It also holds the `completion_service` so it
    // can invalidate the per-org concurrent-limit cache after a PATCH to
    // `/v1/admin/organizations/{org_id}/concurrent-limit`.
    let admin_service = Arc::new(AdminServiceImpl::new(
        admin_repository as Arc<dyn services::admin::AdminRepository>,
        services.models_service as Arc<dyn services::models::ModelsServiceTrait>,
        services.completion_service.clone()
            as Arc<dyn services::completions::CompletionServiceTrait>,
        services::email::sender_from_config(&config.invitation_email)
            .expect("Failed to initialize admin email sender"),
    )) as Arc<dyn services::admin::AdminService + Send + Sync>;

    let github_dispatcher =
        services::github_dispatch::dispatcher_from_config(&config.github_dispatch);

    let infra_service = Arc::new(services::admin::InfraService::new(
        config.infra.machines_url.clone(),
        config.infra.cost_per_host_usd_month,
    ));

    let admin_app_state = AdminAppState {
        admin_service,
        analytics_service: services.analytics_service,
        organization_service: services.organization_service,
        auth_service: auth_state_middleware.auth_service.clone(),
        usage_service: services.usage_service,
        staking_farm_service: services.staking_farm_service,
        config,
        admin_access_token_repository,
        inference_provider_pool: services.inference_provider_pool,
        github_dispatcher,
        infra_service,
    };

    Router::new()
        .route(
            "/admin/models",
            axum::routing::get(admin_list_models).patch(batch_upsert_models),
        )
        .route(
            "/admin/models/deprecate",
            axum::routing::post(deprecate_model),
        )
        .route(
            "/admin/models/pricing-changes",
            axum::routing::get(list_model_pricing_changes),
        )
        .route(
            "/admin/models/pricing-changes/preview",
            axum::routing::post(preview_model_pricing_changes),
        )
        .route(
            "/admin/models/pricing-changes/confirm",
            axum::routing::post(confirm_model_pricing_changes),
        )
        .route(
            "/admin/models/pricing-changes/{id}",
            axum::routing::delete(cancel_model_pricing_change),
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
            "/admin/models/{model_name}/deprecation/preview",
            axum::routing::post(preview_model_deprecation),
        )
        .route(
            "/admin/models/{model_name}/deprecation/confirm",
            axum::routing::post(confirm_model_deprecation),
        )
        .route("/admin/services", axum::routing::post(create_service))
        .route("/admin/services/{id}", axum::routing::patch(update_service))
        .route(
            "/admin/organizations/{org_id}/limits",
            axum::routing::patch(update_organization_limits),
        )
        .route(
            "/admin/organizations/{org_id}/limits/history",
            axum::routing::get(get_organization_limits_history),
        )
        .route(
            "/admin/organizations/{org_id}/usage/balance",
            axum::routing::get(get_admin_organization_balance),
        )
        .route(
            "/admin/organizations/{org_id}/staking/farm",
            axum::routing::get(get_admin_organization_staking_farm),
        )
        .route(
            "/admin/organizations/{org_id}/staking/farm/sync",
            axum::routing::post(sync_admin_organization_staking_farm),
        )
        .route(
            "/admin/organizations/{org_id}/concurrent-limit",
            axum::routing::patch(update_organization_concurrent_limit)
                .get(get_organization_concurrent_limit),
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
        .route(
            "/admin/platform/metrics/timeseries",
            axum::routing::get(get_platform_timeseries),
        )
        .route(
            "/admin/platform/billing-summary",
            axum::routing::get(get_billing_summary),
        )
        .route(
            "/admin/platform/model-revenue",
            axum::routing::get(get_model_revenue),
        )
        .route(
            "/admin/platform/org-revenue",
            axum::routing::get(get_org_revenue),
        )
        .route(
            "/admin/platform/infra-summary",
            axum::routing::get(get_infra_summary),
        )
        .route(
            "/admin/platform/model-consumption-timeseries",
            axum::routing::get(get_model_consumption_timeseries),
        )
        .route(
            "/admin/platform/performance-timeseries",
            axum::routing::get(get_performance_timeseries),
        )
        .route(
            "/admin/platform/revenue-density",
            axum::routing::get(get_revenue_density),
        )
        .route(
            "/admin/invitation-email-deliveries",
            axum::routing::get(list_invitation_email_deliveries),
        )
        .route(
            "/admin/invitation-email-deliveries/{invitation_id}/resend",
            axum::routing::post(resend_invitation_email),
        )
        .route("/admin/users", axum::routing::get(list_users))
        .route(
            "/admin/organizations",
            axum::routing::get(list_organizations),
        )
        .route(
            "/admin/organizations/{org_id}",
            axum::routing::get(get_admin_organization),
        )
        .route(
            "/admin/organizations/{org_id}/members",
            axum::routing::get(list_organization_members),
        )
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
    Router::new()
        .route("/docs", get(swagger_ui_handler))
        .route(
            "/api-docs/openapi.json",
            get(|| async { axum::Json(ApiDoc::openapi()) }),
        )
        // OpenAPI spec + Scalar UI HTML change only on deploy. 5 min fresh +
        // 1 h SWR keeps the docs snappy while still picking up new builds
        // within ~5 minutes.
        .layer(cache_control_layer(
            "public, max-age=300, stale-while-revalidate=3600",
        ))
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

    /// Regression for issue #781 (M1): the Chutes catalog seed must advertise a
    /// NON-EMPTY `supported_features` / `supported_sampling_parameters` so
    /// OpenRouter-style routers don't silently treat the model as supporting
    /// nothing (which disables tool routing).
    #[test]
    fn chutes_seed_advertises_nonempty_capabilities() {
        assert!(
            !CHUTES_SUPPORTED_SAMPLING_PARAMS.is_empty(),
            "Chutes seed must advertise at least one sampling parameter"
        );
        assert!(
            !CHUTES_SUPPORTED_FEATURES.is_empty(),
            "Chutes seed must advertise at least one feature"
        );
        // tool/function-calling is the capability routers gate on; it must be present.
        assert!(
            CHUTES_SUPPORTED_FEATURES.contains(&"tools"),
            "Chutes seed must advertise tool/function-calling support"
        );
    }

    /// The seeded values must stay within OpenRouter's fixed vocabulary so the
    /// auto-seeded row would pass the same validation the admin write path
    /// (`PATCH /v1/admin/models`) enforces. This guards against the two lists
    /// drifting apart.
    #[test]
    fn chutes_seed_values_are_valid_openrouter_vocabulary() {
        for p in CHUTES_SUPPORTED_SAMPLING_PARAMS {
            assert!(
                crate::routes::admin::VALID_SAMPLING_PARAMS.contains(p),
                "seeded sampling parameter '{p}' is not in OpenRouter's vocabulary"
            );
        }
        for f in CHUTES_SUPPORTED_FEATURES {
            assert!(
                crate::routes::admin::VALID_FEATURES.contains(f),
                "seeded feature '{f}' is not in OpenRouter's vocabulary"
            );
        }
    }

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

    #[test]
    fn test_openapi_models_endpoint_is_public() {
        let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
        let models_get = &spec["paths"]["/v1/models"]["get"];

        assert_eq!(
            models_get["security"],
            serde_json::json!([{}]),
            "/v1/models must explicitly override global OpenAPI security"
        );
    }

    #[test]
    fn test_openapi_conversation_action_paths_use_v1_prefix() {
        let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
        let paths = spec["paths"].as_object().unwrap();

        // Pin/unpin and archive/unarchive share path keys with different methods.
        for path in [
            "/v1/conversations/{conversation_id}/archive",
            "/v1/conversations/{conversation_id}/clone",
            "/v1/conversations/{conversation_id}/pin",
        ] {
            assert!(paths.contains_key(path), "missing OpenAPI path: {path}");
        }

        for path in [
            "/conversations/{conversation_id}/archive",
            "/conversations/{conversation_id}/clone",
            "/conversations/{conversation_id}/pin",
        ] {
            assert!(
                !paths.contains_key(path),
                "OpenAPI path is missing /v1 prefix: {path}"
            );
        }
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
                pricing_change_apply_interval_secs: 0,
                ohttp_enabled: false,
            },
            inference_api_key: Some("test-key".to_string()),
            internal_usage_token: None,
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
            invitation_email: config::InvitationEmailConfig::default(),
            otlp: config::OtlpConfig {
                endpoint: "http://localhost:4317".to_string(),
                protocol: "grpc".to_string(),
            },
            cors: config::CorsConfig::default(),
            external_providers: config::ExternalProvidersConfig::default(),
            github_dispatch: config::GitHubDispatchConfig::default(),
            infra: config::InfraConfig::default(),
            staking_farm: config::StakingFarmConfig::default(),
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
                pricing_change_apply_interval_secs: 0,
                ohttp_enabled: false,
            },
            inference_api_key: Some("test-key".to_string()),
            internal_usage_token: None,
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
            invitation_email: config::InvitationEmailConfig::default(),
            otlp: config::OtlpConfig {
                endpoint: "http://localhost:4317".to_string(),
                protocol: "grpc".to_string(),
            },
            cors: config::CorsConfig::default(),
            external_providers: config::ExternalProvidersConfig::default(),
            github_dispatch: config::GitHubDispatchConfig::default(),
            infra: config::InfraConfig::default(),
            staking_farm: config::StakingFarmConfig::default(),
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

    // --- cache_control_layer tests -------------------------------------------
    //
    // These ensure the middleware only attaches a Cache-Control header to
    // successful (2xx) responses, never to 4xx/5xx errors. Without this guard
    // a transient DB failure or 404 could be pinned in CDNs and browsers for
    // the declared TTL.

    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use tower::ServiceExt;

    fn cache_test_app() -> Router {
        async fn ok_handler() -> impl IntoResponse {
            (StatusCode::OK, "ok")
        }
        async fn ok_with_header() -> Response {
            let mut res = (StatusCode::OK, "ok").into_response();
            res.headers_mut()
                .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
            res
        }
        async fn internal_error() -> impl IntoResponse {
            (StatusCode::INTERNAL_SERVER_ERROR, "boom")
        }
        async fn not_found() -> impl IntoResponse {
            (StatusCode::NOT_FOUND, "missing")
        }

        Router::new()
            .route("/ok", get(ok_handler))
            .route("/ok-with-header", get(ok_with_header))
            .route("/err500", get(internal_error))
            .route("/err404", get(not_found))
            .layer(cache_control_layer("public, max-age=30"))
    }

    #[tokio::test]
    async fn cache_control_set_on_2xx() {
        let app = cache_test_app();
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/ok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(CACHE_CONTROL)
                .map(|v| v.to_str().unwrap()),
            Some("public, max-age=30"),
        );
    }

    #[tokio::test]
    async fn cache_control_not_overridden_when_handler_sets_it() {
        let app = cache_test_app();
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/ok-with-header")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(CACHE_CONTROL)
                .map(|v| v.to_str().unwrap()),
            Some("private, no-store"),
        );
    }

    #[tokio::test]
    async fn cache_control_not_set_on_5xx() {
        // The key bug-fix invariant: a 500 must NOT carry a cacheable
        // Cache-Control header, or intermediaries may pin the failure.
        let app = cache_test_app();
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/err500")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            res.headers().get(CACHE_CONTROL).is_none(),
            "Cache-Control must not be set on 5xx responses, got: {:?}",
            res.headers().get(CACHE_CONTROL),
        );
    }

    #[tokio::test]
    async fn cache_control_not_set_on_4xx() {
        // Same risk for transient 404s — admin adds a model 5s later, we must
        // not be serving "missing" from cache for 30s.
        let app = cache_test_app();
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/err404")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert!(
            res.headers().get(CACHE_CONTROL).is_none(),
            "Cache-Control must not be set on 4xx responses, got: {:?}",
            res.headers().get(CACHE_CONTROL),
        );
    }
}
