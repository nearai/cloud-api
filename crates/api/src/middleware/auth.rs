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
        Err(e) => {
            error!("Failed to get admin user by ID: {}", e);
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
) -> Result<Response, StatusCode> {
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

/// Admin authentication middleware - verifies user is authenticated AND has admin access
/// Supports both access tokens (with admin domain check) and admin access tokens
pub async fn admin_middleware(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
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
                    let is_access_token_management = path.starts_with("/admin/access_token");
                    // For access token management endpoints, only allow session-based authentication
                    if is_access_token_management {
                        debug!("Access token management endpoint detected. Forbidden.");
                        return Err(StatusCode::FORBIDDEN);
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
                        Err(e) => {
                            error!("Failed to get admin user for access token: {}", e);
                            Err(StatusCode::INTERNAL_SERVER_ERROR)
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
            Err(StatusCode::UNAUTHORIZED)
        }
    } else {
        Err(StatusCode::UNAUTHORIZED)
    };

    match auth_result {
        Ok(user) => {
            // Check if user has admin access based on email domain
            let is_admin = check_admin_access(&state, &user);

            if !is_admin {
                error!(
                    "User {} ({}) attempted admin action without admin privileges",
                    user.id, user.email
                );
                return Err(StatusCode::FORBIDDEN);
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
) -> Result<database::models::AdminAccessToken, StatusCode> {
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
            debug!("Admin access token not found or inactive");
            Err(StatusCode::UNAUTHORIZED)
        }
        Err(e) => {
            error!("Failed to validate admin access token: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn authenticate_session_access(
    state: &AuthState,
    token: String, // jwt
) -> Result<DbUser, StatusCode> {
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
            Err(e) => {
                error!("Failed to validate session via auth service: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    // No fallback available, session is invalid
    debug!("Invalid or expired session access token");
    Err(StatusCode::UNAUTHORIZED)
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
            Err(e) => {
                error!("Failed to validate session via auth service: {}", e);
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
        Err(e) => {
            error!("Failed to validate API key: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(crate::models::ErrorResponse::new(
                    "Failed to validate API key".to_string(),
                    "internal_error".to_string(),
                )),
            ))
        }
    }
}

/// Authenticate using API key and resolve workspace/organization context
async fn authenticate_api_key_with_context(
    state: &AuthState,
    api_key: &str,
) -> Result<AuthenticatedApiKey, (StatusCode, axum::Json<crate::models::ErrorResponse>)> {
    // First validate the API key
    let validated_api_key = authenticate_api_key(state, api_key).await?;

    debug!(
        "Resolving workspace and organization for API key: {:?}",
        validated_api_key.id
    );

    // Clone workspace_id to avoid partial move
    let workspace_id = validated_api_key.workspace_id.clone();

    // Get workspace with organization info
    match state
        .workspace_repository
        .get_workspace_with_organization(workspace_id)
        .await
    {
        Ok(Some((workspace, organization))) => {
            debug!(
                "Resolved workspace: {} and organization: {} for API key",
                workspace.name, organization.name
            );
            Ok(AuthenticatedApiKey {
                api_key: validated_api_key,
                workspace,
                organization,
            })
        }
        Ok(None) => {
            error!("Workspace not found for API key");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "Workspace not found for API key".to_string(),
                    "invalid_api_key".to_string(),
                )),
            ))
        }
        Err(e) => {
            error!("Failed to resolve workspace/organization: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(crate::models::ErrorResponse::new(
                    "Failed to resolve workspace/organization".to_string(),
                    "internal_error".to_string(),
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
    pub admin_access_token_repository: Arc<database::repositories::AdminAccessTokenRepository>,
    pub admin_domains: Vec<String>,
    pub encoding_key: String,
}

impl AuthState {
    pub fn new(
        oauth_manager: Arc<OAuthManager>,
        auth_service: Arc<dyn AuthServiceTrait>,
        workspace_repository: Arc<dyn services::workspace::WorkspaceRepository>,
        admin_access_token_repository: Arc<database::repositories::AdminAccessTokenRepository>,
        admin_domains: Vec<String>,
        encoding_key: String,
    ) -> Self {
        Self {
            oauth_manager,
            auth_service,
            workspace_repository,
            admin_access_token_repository,
            admin_domains,
            encoding_key,
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
