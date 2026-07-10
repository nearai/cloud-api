use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use database::User as DbUser;
use services::auth::{AuthError, AuthServiceTrait, OAuthManager, SessionToken};
use services::common::REPORTING_TOKEN_PREFIX;
use services::reporting_tokens::{ReportingTokenScope, ValidatedOrganizationReportingToken};
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
            debug!(admin_user_id = %user.id, "Found admin user");
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

#[derive(Clone, Debug)]
pub struct AuthenticatedReportingToken {
    pub id: uuid::Uuid,
    pub organization_id: uuid::Uuid,
    pub token_prefix: String,
    pub scope: ReportingTokenScope,
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

    tracing::debug!(
        authorization_present = auth_header.is_some(),
        "Auth API KEY middleware"
    );

    let auth_result = if let Some(auth_value) = auth_header {
        debug!("Found Authorization header");
        if let Some(token) = auth_value.strip_prefix("Bearer ") {
            authenticate_api_key(&state, token).await
        } else {
            debug!("Authorization header uses unsupported scheme");
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
            debug!(api_key_id = %api_key.id.0, "Adding API key to request");
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

    tracing::debug!(
        authorization_present = auth_header.is_some(),
        "Auth API KEY with workspace middleware"
    );

    let auth_result = if let Some(auth_value) = auth_header {
        debug!("Found Authorization header");
        if let Some(token) = auth_value.strip_prefix("Bearer ") {
            authenticate_api_key_with_context(&state, token).await
        } else {
            debug!("Authorization header uses unsupported scheme");
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
        authorization_present = auth_header.is_some(),
        "Auth middleware (session access token only)"
    );

    let auth_result = if let Some(auth_value) = auth_header {
        debug!("Found Authorization header");
        if let Some(token) = auth_value.strip_prefix("Bearer ") {
            debug!("Extracted Bearer token");
            authenticate_session_access(&state, token.to_string()).await
        } else {
            debug!("Authorization header uses unsupported scheme");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "Invalid authorization header format".to_string(),
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

pub async fn auth_middleware_with_reporting_token(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, axum::Json<crate::models::ErrorResponse>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok());

    tracing::debug!(
        has_authorization = auth_header.is_some(),
        "Auth reporting token middleware"
    );

    let auth_result = if let Some(auth_value) = auth_header {
        debug!("Found Authorization header");
        if let Some(token) = auth_value.strip_prefix("Bearer ") {
            authenticate_reporting_token(&state, token).await
        } else {
            debug!("Authorization header uses unsupported scheme");
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
        Ok(reporting_token) => {
            let mut request = request;
            request.extensions_mut().insert(reporting_token);
            Ok(next.run(request).await)
        }
        Err(error) => Err(error),
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

    let user_agent = request
        .headers()
        .get("User-Agent")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    tracing::debug!(
        authorization_present = auth_header.is_some(),
        "Admin auth middleware"
    );

    let auth_result = if let Some(auth_value) = auth_header {
        debug!("Found Authorization header");
        if let Some(token) = auth_value.strip_prefix("Bearer ") {
            debug!("Extracted Bearer token for admin auth");

            // Check if this looks like an admin access token (starts with "adm_")
            // Admin access tokens should ONLY be validated as admin tokens, no fallback
            if token.starts_with("adm_") {
                match authenticate_admin_access_token(&state, token, user_agent.as_deref()).await {
                    Ok(admin_token) => {
                        debug!(
                            admin_access_token_id = %admin_token.id,
                            authenticated = true,
                            "Authenticated via admin access token"
                        );

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
                                    admin_user_id = %admin_user.id,
                                    admin_access_token_id = %admin_token.id,
                                    authenticated = true,
                                    "Retrieved admin user for access token"
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
                    Err(err) => {
                        debug!("Admin access token validation failed: {:?}", err);
                        // Don't fall back to session token for admin access tokens
                        // If it looks like an admin token but validation fails, reject it
                        Err(err)
                    }
                }
            } else {
                // Not an admin access token, try as session token
                debug!("Token does not appear to be an admin access token, trying session token");
                authenticate_session_access(&state, token.to_string()).await
            }
        } else {
            debug!("Authorization header uses unsupported scheme");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "Invalid authorization header format".to_string(),
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
                tracing::warn!("User attempted admin action without admin privileges");
                return Err((
                    StatusCode::FORBIDDEN,
                    axum::Json(crate::models::ErrorResponse::new(
                        "No admin privileges".to_string(),
                        "forbidden".to_string(),
                    )),
                ));
            }

            debug!(admin_user_id = %user.id, authenticated = true, "Admin access granted");

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
    token: &str,
    user_agent: Option<&str>,
) -> Result<
    database::models::AdminAccessToken,
    (StatusCode, axum::Json<crate::models::ErrorResponse>),
> {
    debug!("Authenticating admin access token");

    match state
        .admin_access_token_repository
        .validate(token, user_agent)
        .await
    {
        Ok(Some(admin_token)) => {
            debug!(
                admin_access_token_id = %admin_token.id,
                validated = true,
                "Admin access token validated successfully"
            );
            Ok(admin_token)
        }
        Ok(None) => {
            debug!("Invalid admin access token: not found, expired, or User-Agent mismatches");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "Invalid admin access token".to_string(),
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
    if token.starts_with(REPORTING_TOKEN_PREFIX) {
        debug!("Reporting token rejected by session middleware");
        return Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(crate::models::ErrorResponse::new(
                "Invalid or expired access token".to_string(),
                "unauthorized".to_string(),
            )),
        ));
    }

    debug!("Authenticating session access token");
    // Use auth service

    let auth_service = &state.auth_service;
    debug!("Validating session via auth service with token");
    {
        match auth_service
            .validate_session_access(token, state.encoding_key.clone())
            .await
        {
            Ok(user) => {
                debug!(user_id = %user.id.0, authenticated = true, "Authenticated user via session");
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

    // Try to get user agent from header
    let user_agent = request
        .headers()
        .get("User-Agent")
        .and_then(|h| h.to_str().ok());

    tracing::debug!(
        authorization_present = auth_header.is_some(),
        user_agent_present = user_agent.is_some(),
        "Auth middleware (refresh token)"
    );

    let auth_result = if let Some(auth_value) = auth_header {
        debug!("Found Authorization header");
        if let Some(token) = auth_value.strip_prefix("Bearer ") {
            debug!("Extracted Bearer token");
            if token.starts_with(REPORTING_TOKEN_PREFIX) {
                debug!("Reporting token rejected by refresh middleware");
                return Err(StatusCode::UNAUTHORIZED);
            }
            if let Some(user_agent_value) = user_agent {
                authenticate_session_refresh(
                    &state,
                    SessionToken(token.to_string()),
                    user_agent_value,
                )
                .await
            } else {
                debug!("Missing User-Agent header");
                Err(StatusCode::UNAUTHORIZED)
            }
        } else {
            debug!("Authorization header uses unsupported scheme");
            Err(StatusCode::UNAUTHORIZED)
        }
    } else {
        Err(StatusCode::UNAUTHORIZED)
    };

    match auth_result {
        Ok((session, user)) => {
            // Clone request to add extension
            let mut request = request;
            request
                .extensions_mut()
                .insert((session, AuthenticatedUser(user)));
            Ok(next.run(request).await)
        }
        Err(status) => Err(status),
    }
}

/// Authenticate using refresh token and user agent validation
async fn authenticate_session_refresh(
    state: &AuthState,
    token: SessionToken,
    user_agent: &str,
) -> Result<(services::auth::Session, DbUser), StatusCode> {
    debug!("Authenticating session refresh token");
    // Use auth service to validate session with refresh token and user agent
    {
        let auth_service = &state.auth_service;
        debug!("Validating session via auth service with refresh token and user agent");
        match auth_service
            .validate_session_refresh(token, user_agent)
            .await
        {
            Ok((session, user)) => {
                debug!(user_id = %user.id.0, authenticated = true, "Authenticated user via session");
                return Ok((session, convert_user_to_db_user(user)));
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
    debug!("Calling auth service to validate API key");

    match auth_service.validate_api_key(api_key.to_string()).await {
        Ok(api_key) => {
            debug!(api_key_id = %api_key.id.0, "Authenticated via API key");
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
            tracing::warn!("API key references non-existent user");
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

async fn authenticate_reporting_token(
    state: &AuthState,
    token: &str,
) -> Result<AuthenticatedReportingToken, (StatusCode, axum::Json<crate::models::ErrorResponse>)> {
    debug!("Calling reporting token repository to validate token");

    match state.reporting_token_repository.validate(token).await {
        Ok(Some(validated)) => Ok(reporting_token_extension(validated)),
        Ok(None) => {
            debug!("Invalid or expired reporting token");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "Invalid or expired reporting token".to_string(),
                    "invalid_reporting_token".to_string(),
                )),
            ))
        }
        Err(_) => {
            error!("Failed to validate reporting token");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(crate::models::ErrorResponse::new(
                    "Failed to validate reporting token".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

fn reporting_token_extension(
    validated: ValidatedOrganizationReportingToken,
) -> AuthenticatedReportingToken {
    AuthenticatedReportingToken {
        id: validated.id,
        organization_id: validated.organization_id,
        token_prefix: validated.token_prefix,
        scope: validated.scope,
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
                "Resolved workspace_id={} organization_id={} workspace_active={} organization_active={} for API key",
                workspace.id, organization.id, workspace.is_active, organization.is_active
            );
            Ok(AuthenticatedApiKey {
                api_key: validated_api_key,
                workspace,
                organization,
            })
        }
        Ok(None) => {
            tracing::warn!("Workspace not found for API key");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "Workspace not found for API key".to_string(),
                    "invalid_api_key".to_string(),
                )),
            ))
        }
        Err(_) => {
            error!("Failed to resolve workspace/organization");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(crate::models::ErrorResponse::new(
                    "Failed to resolve workspace/organization".to_string(),
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
    pub reporting_token_repository:
        Arc<dyn services::reporting_tokens::OrganizationReportingTokenRepository>,
    pub admin_access_token_repository: Arc<database::repositories::AdminAccessTokenRepository>,
    pub admin_domains: Vec<String>,
    pub encoding_key: String,
}

impl AuthState {
    pub fn new(
        oauth_manager: Arc<OAuthManager>,
        auth_service: Arc<dyn AuthServiceTrait>,
        workspace_repository: Arc<dyn services::workspace::WorkspaceRepository>,
        reporting_token_repository: Arc<
            dyn services::reporting_tokens::OrganizationReportingTokenRepository,
        >,
        admin_access_token_repository: Arc<database::repositories::AdminAccessTokenRepository>,
        admin_domains: Vec<String>,
        encoding_key: String,
    ) -> Self {
        Self {
            oauth_manager,
            auth_service,
            workspace_repository,
            reporting_token_repository,
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
        auth_provider: user.auth_provider,
        provider_user_id: user.provider_user_id,
        tokens_revoked_at: user.tokens_revoked_at,
    }
}
