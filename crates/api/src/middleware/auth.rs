use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use database::User as DbUser;
use services::auth::{AuthError, AuthServiceTrait, OAuthManager, SessionToken};
use std::sync::Arc;
use tracing::{debug, error};

/// Authenticated user information passed to route handlers
#[derive(Clone)]
pub struct AuthenticatedUser(pub DbUser);

/// Authenticated admin user (extends AuthenticatedUser)
#[derive(Clone)]
pub struct AdminUser(pub DbUser);

/// Get admin user by ID from database
async fn get_admin_user_by_id(
    state: &AuthState,
    user_id: uuid::Uuid,
) -> Result<DbUser, StatusCode> {
    debug!("Querying admin user by ID: {}", user_id);

    match state
        .auth_service
        .get_user_by_id(services::auth::UserId(user_id))
        .await
    {
        Ok(user) => {
            debug!("Found admin user: {}", user.email);
            Ok(convert_user_to_db_user(user))
        }
        Err(_) => {
            error!("Failed to get admin user by ID");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Authenticated API key with workspace and organization context
#[derive(Clone, Debug)]
pub struct AuthenticatedApiKey {
    pub api_key: services::workspace::ApiKey,
    pub workspace: services::workspace::Workspace,
    pub organization: services::organization::Organization,
}

pub async fn auth_middleware_with_api_key(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, axum::Json<crate::models::ErrorResponse>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok());

    tracing::debug!("Auth API KEY middleware: {:?}", auth_header);

    let auth_result = if let Some(auth_value) = auth_header {
        debug!("Found Authorization header: {}", auth_value);
        if let Some(token) = auth_value.strip_prefix("Bearer ") {
            authenticate_api_key(&state, token).await
        } else {
            debug!("Authorization header does not start with 'Bearer '");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "Invalid authorization header format".to_string(),
                    "invalid_auth_header".to_string(),
                )),
            ))
        }
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(crate::models::ErrorResponse::new(
                "Missing authorization header".to_string(),
                "missing_auth_header".to_string(),
            )),
        ))
    };

    match auth_result {
        Ok(api_key) => {
            // Clone request to add extension
            debug!("Adding API key to request: {:?}", api_key);
            let mut request = request;
            request.extensions_mut().insert(api_key);
            Ok(next.run(request).await)
        }
        Err(error) => Err(error),
    }
}

/// API Key middleware with workspace/organization context resolution
pub async fn auth_middleware_with_workspace_context(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, axum::Json<crate::models::ErrorResponse>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok());

    tracing::debug!("Auth API KEY with workspace middleware: {:?}", auth_header);

    let auth_result = if let Some(auth_value) = auth_header {
        debug!("Found Authorization header: {}", auth_value);
        if let Some(token) = auth_value.strip_prefix("Bearer ") {
            authenticate_api_key_with_context(&state, token).await
        } else {
            debug!("Authorization header does not start with 'Bearer '");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "Invalid authorization header format".to_string(),
                    "invalid_auth_header".to_string(),
                )),
            ))
        }
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(crate::models::ErrorResponse::new(
                "Missing authorization header".to_string(),
                "missing_auth_header".to_string(),
            )),
        ))
    };

    match auth_result {
        Ok(authenticated_api_key) => {
            debug!("Adding authenticated API key with workspace context to request");
            let mut request = request;
            request.extensions_mut().insert(authenticated_api_key);
            Ok(next.run(request).await)
        }
        Err(error) => Err(error),
    }
}

/// Authentication middleware that validates session tokens only
pub async fn auth_middleware(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, axum::Json<crate::models::ErrorResponse>)> {
    // Try to extract authentication from various sources
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok());

    tracing::debug!(
        "Auth middleware (session access token only): {:?}",
        auth_header
    );

    let auth_result = if let Some(auth_value) = auth_header {
        debug!("Found Authorization header: {}", auth_value);
        if let Some(token) = auth_value.strip_prefix("Bearer ") {
            debug!("Extracted Bearer token: {}", token);
            authenticate_session_access(&state, token.to_string()).await
        } else {
            debug!("Authorization header does not start with 'Bearer '");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "Authorization header does not start with 'Bearer '".to_string(),
                    "unauthorized".to_string(),
                )),
            ))
        }
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(crate::models::ErrorResponse::new(
                "Missing authorization".to_string(),
                "unauthorized".to_string(),
            )),
        ))
    };

    match auth_result {
        Ok(user) => {
            // Clone request to add extension
            let mut request = request;
            request.extensions_mut().insert(AuthenticatedUser(user));
            Ok(next.run(request).await)
        }
        Err(status) => Err(status),
    }
}

/// Admin authentication middleware - verifies user is authenticated AND has admin access
/// Supports both access tokens (with admin domain check) and admin access tokens
pub async fn admin_middleware(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, axum::Json<crate::models::ErrorResponse>)> {
    // Try to extract authentication from various sources
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok());

    tracing::debug!("Admin auth middleware: {:?}", auth_header);

    let auth_result = if let Some(auth_value) = auth_header {
        debug!("Found Authorization header: {}", auth_value);
        if let Some(token) = auth_value.strip_prefix("Bearer ") {
            debug!("Extracted Bearer token for admin auth: {}", token);

            // Try admin access token first
            match authenticate_admin_access_token(&state, token.to_string()).await {
                Ok(admin_token) => {
                    debug!("Authenticated via admin access token: {}", admin_token.name);

                    // Check if this is an admin access token management endpoint
                    let path = request.uri().path();
                    let is_access_token_management = path.starts_with("/admin/access-tokens");
                    // For access token management endpoints, only allow session-based authentication
                    if is_access_token_management {
                        debug!("Access token management endpoint detected. Forbidden.");
                        return Err((
                            StatusCode::FORBIDDEN,
                            axum::Json(crate::models::ErrorResponse::new(
                                "Access token management endpoint detected".to_string(),
                                "forbidden".to_string(),
                            )),
                        ));
                    }

                    // Query the actual admin user from database
                    match get_admin_user_by_id(&state, admin_token.created_by_user_id).await {
                        Ok(admin_user) => {
                            debug!(
                                "Retrieved admin user: {} for access token: {}",
                                admin_user.email, admin_token.name
                            );
                            Ok(admin_user)
                        }
                        Err(_) => {
                            error!("Failed to get admin user for access token");
                            Err((
                                StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(crate::models::ErrorResponse::new(
                                    "Failed to get admin user for access token".to_string(),
                                    "internal_server_error".to_string(),
                                )),
                            ))
                        }
                    }
                }
                Err(_) => {
                    // Fall back to session token authentication
                    debug!("Admin access token validation failed, trying session token");
                    authenticate_session_access(&state, token.to_string()).await
                }
            }
        } else {
            debug!("Authorization header does not start with 'Bearer '");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "Authorization header does not start with 'Bearer '".to_string(),
                    "unauthorized".to_string(),
                )),
            ))
        }
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(crate::models::ErrorResponse::new(
                "Missing authorization".to_string(),
                "unauthorized".to_string(),
            )),
        ))
    };

    match auth_result {
        Ok(user) => {
            // Check if user has admin access based on email domain
            let is_admin = check_admin_access(&state, &user);

            if !is_admin {
                error!("User attempted admin action without admin privileges");
                return Err((
                    StatusCode::FORBIDDEN,
                    axum::Json(crate::models::ErrorResponse::new(
                        "No admin privileges".to_string(),
                        "forbidden".to_string(),
                    )),
                ));
            }

            debug!(
                "Admin access granted for user: {} ({})",
                user.id, user.email
            );

            // Add both AuthenticatedUser and AdminUser extensions
            let mut request = request;
            request
                .extensions_mut()
                .insert(AuthenticatedUser(user.clone()));
            request.extensions_mut().insert(AdminUser(user));
            Ok(next.run(request).await)
        }
        Err(status) => Err(status),
    }
}

/// Check if a user has admin access based on their email domain
fn check_admin_access(state: &AuthState, user: &DbUser) -> bool {
    if state.admin_domains.is_empty() {
        return false;
    }

    // Extract domain from email (everything after @)
    if let Some(domain) = user.email.split('@').nth(1) {
        state
            .admin_domains
            .iter()
            .any(|admin_domain| domain.eq_ignore_ascii_case(admin_domain))
    } else {
        false
    }
}

/// Authenticate admin access token
async fn authenticate_admin_access_token(
    state: &AuthState,
    token: String,
) -> Result<
    database::models::AdminAccessToken,
    (StatusCode, axum::Json<crate::models::ErrorResponse>),
> {
    debug!("Authenticating admin access token: {}", token);

    match state.admin_access_token_repository.validate(&token).await {
        Ok(Some(admin_token)) => {
            debug!(
                "Admin access token validated successfully: {}",
                admin_token.name
            );
            Ok(admin_token)
        }
        Ok(None) => {
            debug!("Admin access token not found or expired");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "Admin access token not found or expired".to_string(),
                    "unauthorized".to_string(),
                )),
            ))
        }
        Err(_) => {
            error!("Failed to validate admin access token");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(crate::models::ErrorResponse::new(
                    "Failed to validate admin access token".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

async fn authenticate_session_access(
    state: &AuthState,
    token: String, // jwt
) -> Result<DbUser, (StatusCode, axum::Json<crate::models::ErrorResponse>)> {
    debug!("Authenticating session access token: {}", token);
    // Use auth service

    let auth_service = &state.auth_service;
    debug!("Validating session via auth service with token");
    {
        match auth_service
            .validate_session_access(token, state.encoding_key.clone())
            .await
        {
            Ok(user) => {
                debug!("Authenticated user {} via session", user.email);
                return Ok(convert_user_to_db_user(user));
            }
            Err(AuthError::SessionNotFound) | Err(AuthError::UserNotFound) => {
                debug!("Session not found in auth service, trying OAuth manager");
                // Fall through to OAuth manager
            }
            Err(_) => {
                error!("Failed to validate session via auth service");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(crate::models::ErrorResponse::new(
                        "Failed to validate session via auth service".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ));
            }
        }
    }

    // No fallback available, session is invalid
    debug!("Invalid or expired session access token");
    Err((
        StatusCode::UNAUTHORIZED,
        axum::Json(crate::models::ErrorResponse::new(
            "Invalid or expired access token".to_string(),
            "unauthorized".to_string(),
        )),
    ))
}

pub async fn refresh_middleware(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Try to extract authentication from various sources
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok());

    tracing::debug!(
        "Auth middleware (session refresh token only): {:?}",
        auth_header
    );

    let auth_result = if let Some(auth_value) = auth_header {
        debug!("Found Authorization header: {}", auth_value);
        if let Some(token) = auth_value.strip_prefix("Bearer ") {
            debug!("Extracted Bearer token: {}", token);
            authenticate_session_refresh(&state, SessionToken(token.to_string())).await
        } else {
            debug!("Authorization header does not start with 'Bearer '");
            Err(StatusCode::UNAUTHORIZED)
        }
    } else {
        Err(StatusCode::UNAUTHORIZED)
    };

    match auth_result {
        Ok(user) => {
            // Clone request to add extension
            let mut request = request;
            request.extensions_mut().insert(AuthenticatedUser(user));
            Ok(next.run(request).await)
        }
        Err(status) => Err(status),
    }
}

/// Authenticate using session token
async fn authenticate_session_refresh(
    state: &AuthState,
    token: SessionToken,
) -> Result<DbUser, StatusCode> {
    debug!("Authenticating session refresh token: {}", token);
    // Use auth service
    {
        let auth_service = &state.auth_service;
        debug!("Validating session via auth service with token");
        match auth_service.validate_session_refresh(token).await {
            Ok(user) => {
                debug!("Authenticated user {} via session", user.email);
                return Ok(convert_user_to_db_user(user));
            }
            Err(AuthError::SessionNotFound) | Err(AuthError::UserNotFound) => {
                debug!("Session not found in auth service, trying OAuth manager");
                // Fall through to OAuth manager
            }
            Err(_) => {
                error!("Failed to validate session via auth service");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    // No fallback available, session is invalid
    debug!("Invalid or expired session refresh token");
    Err(StatusCode::UNAUTHORIZED)
}

/// Authenticate using API key
async fn authenticate_api_key(
    state: &AuthState,
    api_key: &str,
) -> Result<services::workspace::ApiKey, (StatusCode, axum::Json<crate::models::ErrorResponse>)> {
    let auth_service = &state.auth_service;
    debug!("Calling auth service to validate API key: {}", api_key);

    match auth_service.validate_api_key(api_key.to_string()).await {
        Ok(api_key) => {
            debug!("Authenticated via API key: {:?}", api_key);
            Ok(api_key)
        }
        Err(AuthError::Unauthorized) => {
            debug!("Invalid or expired API key");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "Invalid or expired API key".to_string(),
                    "invalid_api_key".to_string(),
                )),
            ))
        }
        Err(AuthError::UserNotFound) => {
            error!("API key references non-existent user");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "API key references non-existent user".to_string(),
                    "invalid_api_key".to_string(),
                )),
            ))
        }
        Err(_) => {
            error!("Failed to validate API key");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(crate::models::ErrorResponse::new(
                    "Failed to validate API key".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Authenticate using API key and resolve workspace/organization context
/// This uses a single optimized JOIN query instead of multiple sequential queries
/// with in-memory caching to reduce database load
async fn authenticate_api_key_with_context(
    state: &AuthState,
    api_key: &str,
) -> Result<AuthenticatedApiKey, (StatusCode, axum::Json<crate::models::ErrorResponse>)> {
    let span = tracing::debug_span!("authenticate_api_key_with_context");
    let _enter = span.enter();
    let start = std::time::Instant::now();

    // Check cache first for hot path optimization
    if let Some(cached) = state.api_key_cache.get(api_key).await {
        let elapsed = start.elapsed();
        debug!(
            elapsed_ms = elapsed.as_millis(),
            "API key cache hit in {:?}", elapsed
        );
        return Ok((*cached).clone());
    }

    debug!("API key cache miss, querying database");

    // Use optimized combined query that JOINs api_keys, workspaces, and organizations
    match state
        .api_key_repository
        .validate_with_workspace_and_org(api_key)
        .await
    {
        Ok(Some((db_api_key, db_workspace, db_organization))) => {
            debug!(
                "Authenticated API key: {:?}, workspace: {}, organization: {}",
                db_api_key.id, db_workspace.name, db_organization.name
            );

            // Convert database models to service models
            let api_key_service = services::workspace::ApiKey {
                id: services::workspace::ApiKeyId(db_api_key.id.to_string()),
                key: None, // Don't expose the actual key in auth response
                key_prefix: db_api_key.key_prefix.clone(),
                workspace_id: services::workspace::WorkspaceId(db_api_key.workspace_id),
                created_by_user_id: services::auth::UserId(db_api_key.created_by_user_id),
                name: db_api_key.name.clone(),
                created_at: db_api_key.created_at,
                expires_at: db_api_key.expires_at,
                last_used_at: db_api_key.last_used_at,
                is_active: db_api_key.is_active,
                deleted_at: None, // Active keys in auth won't have deleted_at
                spend_limit: db_api_key.spend_limit,
                usage: Some(db_api_key.usage),
            };

            let workspace_service = services::workspace::Workspace {
                id: services::workspace::WorkspaceId(db_workspace.id),
                name: db_workspace.name.clone(),
                display_name: db_workspace.display_name.clone(),
                description: db_workspace.description.clone(),
                organization_id: services::organization::OrganizationId(
                    db_workspace.organization_id,
                ),
                created_by_user_id: services::auth::UserId(db_workspace.created_by_user_id),
                created_at: db_workspace.created_at,
                updated_at: db_workspace.updated_at,
                is_active: db_workspace.is_active,
                settings: db_workspace.settings.clone(),
            };

            let organization_service = services::organization::Organization {
                id: services::organization::OrganizationId(db_organization.id),
                name: db_organization.name.clone(),
                description: db_organization.description.clone(),
                owner_id: services::auth::UserId(uuid::Uuid::nil()), // We don't have owner_id in this query
                created_at: db_organization.created_at,
                updated_at: db_organization.updated_at,
                is_active: db_organization.is_active,
                settings: db_organization
                    .settings
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({})),
            };

            let authenticated = AuthenticatedApiKey {
                api_key: api_key_service,
                workspace: workspace_service,
                organization: organization_service,
            };

            // Store in cache for future requests
            state
                .api_key_cache
                .insert(api_key.to_string(), Arc::new(authenticated.clone()))
                .await;

            let elapsed = start.elapsed();
            debug!(
                elapsed_ms = elapsed.as_millis(),
                "API key authenticated in {:?}", elapsed
            );
            Ok(authenticated)
        }
        Ok(None) => {
            debug!("Invalid or expired API key");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "Invalid or expired API key".to_string(),
                    "invalid_api_key".to_string(),
                )),
            ))
        }
        Err(_) => {
            error!("Failed to validate API key");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(crate::models::ErrorResponse::new(
                    "Failed to validate API key".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// State for authentication middleware
#[derive(Clone)]
pub struct AuthState {
    pub oauth_manager: Arc<OAuthManager>,
    pub auth_service: Arc<dyn AuthServiceTrait>,
    pub workspace_repository: Arc<dyn services::workspace::WorkspaceRepository>,
    pub api_key_repository: Arc<database::repositories::ApiKeyRepository>,
    pub admin_access_token_repository: Arc<database::repositories::AdminAccessTokenRepository>,
    pub admin_domains: Vec<String>,
    pub encoding_key: String,
    pub api_key_cache: super::cache::ApiKeyCache,
}

impl AuthState {
    pub fn new(
        oauth_manager: Arc<OAuthManager>,
        auth_service: Arc<dyn AuthServiceTrait>,
        workspace_repository: Arc<dyn services::workspace::WorkspaceRepository>,
        api_key_repository: Arc<database::repositories::ApiKeyRepository>,
        admin_access_token_repository: Arc<database::repositories::AdminAccessTokenRepository>,
        admin_domains: Vec<String>,
        encoding_key: String,
        api_key_cache: super::cache::ApiKeyCache,
    ) -> Self {
        Self {
            oauth_manager,
            auth_service,
            workspace_repository,
            api_key_repository,
            admin_access_token_repository,
            admin_domains,
            encoding_key,
            api_key_cache,
        }
    }
}

/// Convert service domain User to database User for backward compatibility
fn convert_user_to_db_user(user: services::auth::User) -> DbUser {
    DbUser {
        id: user.id.0,
        email: user.email,
        username: user.username,
        display_name: user.display_name,
        avatar_url: user.avatar_url,
        created_at: user.created_at,
        updated_at: user.updated_at,
        last_login_at: user.last_login,
        is_active: user.is_active,
        auth_provider: "oauth".to_string(), // Default for now
        provider_user_id: user.id.0.to_string(),
        tokens_revoked_at: user.tokens_revoked_at,
    }
}
