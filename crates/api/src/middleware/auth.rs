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

/// Authenticated API key with workspace and organization context
#[derive(Clone, Debug)]
pub struct AuthenticatedApiKey {
    pub api_key: services::auth::ApiKey,
    pub workspace: services::auth::ports::Workspace,
    pub organization: services::organization::Organization,
}

pub async fn auth_middleware_with_api_key(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
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
            Err(StatusCode::UNAUTHORIZED)
        }
    } else {
        Err(StatusCode::UNAUTHORIZED)
    };

    match auth_result {
        Ok(api_key) => {
            // Clone request to add extension
            debug!("Adding API key to request: {:?}", api_key);
            let mut request = request;
            request.extensions_mut().insert(api_key);
            Ok(next.run(request).await)
        }
        Err(status) => Err(status),
    }
}

/// API Key middleware with workspace/organization context resolution
pub async fn auth_middleware_with_workspace_context(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
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
            Err(StatusCode::UNAUTHORIZED)
        }
    } else {
        Err(StatusCode::UNAUTHORIZED)
    };

    match auth_result {
        Ok(authenticated_api_key) => {
            debug!("Adding authenticated API key with workspace context to request");
            let mut request = request;
            request.extensions_mut().insert(authenticated_api_key);
            Ok(next.run(request).await)
        }
        Err(status) => Err(status),
    }
}

/// Authentication middleware that validates session tokens or API keys
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

    tracing::debug!("Auth middleware: {:?}", auth_header);

    let auth_result = if let Some(auth_value) = auth_header {
        debug!("Found Authorization header: {}", auth_value);
        if let Some(token) = auth_value.strip_prefix("Bearer ") {
            debug!("Extracted Bearer token: {}", token);
            // Pass the token directly as SessionToken
            let session_token = SessionToken(token.to_string());
            authenticate_session(&state, session_token).await
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
async fn authenticate_session(
    state: &AuthState,
    token: SessionToken,
) -> Result<DbUser, StatusCode> {
    debug!("Authenticating session token: {}", token);
    // Use auth service
    {
        let auth_service = &state.auth_service;
        debug!("Validating session via auth service with token");
        match auth_service.validate_session(token).await {
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
    debug!("Invalid or expired session token");
    Err(StatusCode::UNAUTHORIZED)
}

/// Authenticate using API key
async fn authenticate_api_key(
    state: &AuthState,
    api_key: &str,
) -> Result<services::auth::ApiKey, StatusCode> {
    let auth_service = &state.auth_service;
    debug!("Calling auth service to validate API key: {}", api_key);

    match auth_service.validate_api_key(api_key.to_string()).await {
        Ok(api_key) => {
            debug!("Authenticated via API key: {:?}", api_key);
            Ok(api_key)
        }
        Err(AuthError::Unauthorized) => {
            debug!("Invalid or expired API key");
            Err(StatusCode::UNAUTHORIZED)
        }
        Err(AuthError::UserNotFound) => {
            error!("API key references non-existent user");
            Err(StatusCode::UNAUTHORIZED)
        }
        Err(e) => {
            error!("Failed to validate API key: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Authenticate using API key and resolve workspace/organization context
async fn authenticate_api_key_with_context(
    state: &AuthState,
    api_key: &str,
) -> Result<AuthenticatedApiKey, StatusCode> {
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
            Err(StatusCode::UNAUTHORIZED)
        }
        Err(e) => {
            error!("Failed to resolve workspace/organization: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// State for authentication middleware
#[derive(Clone)]
pub struct AuthState {
    pub oauth_manager: Arc<OAuthManager>,
    pub auth_service: Arc<dyn AuthServiceTrait>,
    pub workspace_repository: Arc<dyn services::auth::ports::WorkspaceRepository>,
}

impl AuthState {
    pub fn new(
        oauth_manager: Arc<OAuthManager>,
        auth_service: Arc<dyn AuthServiceTrait>,
        workspace_repository: Arc<dyn services::auth::ports::WorkspaceRepository>,
    ) -> Self {
        Self {
            oauth_manager,
            auth_service,
            workspace_repository,
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
    }
}
