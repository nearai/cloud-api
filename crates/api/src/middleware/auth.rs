use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use database::{Database, User as DbUser};
use domain::auth::OAuthManager;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error};
use uuid::Uuid;

/// Authenticated user information passed to route handlers
#[derive(Clone)]
pub struct AuthenticatedUser(pub DbUser);

/// Authentication middleware that validates session tokens or API keys
pub async fn auth_middleware(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Try to extract authentication from various sources
    let auth_header = request.headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok());
    
    let auth_result = if let Some(auth_value) = auth_header {
        if let Some(token) = auth_value.strip_prefix("Bearer ") {
        
            // Check if it's an API key (starts with "sk_")
            if token.starts_with("sk_") {
                authenticate_api_key(&state, token).await
            } else {
                // Treat as session token
                authenticate_session(&state, token).await
            }
        } else {
            Err(StatusCode::UNAUTHORIZED)
        }
    } else if let Some(cookie_str) = request.headers()
            .get("cookie")
            .and_then(|h| h.to_str().ok()) {
        
        // Parse cookies manually
        let session_id = cookie_str
            .split(';')
            .filter_map(|c| {
                let parts: Vec<&str> = c.trim().splitn(2, '=').collect();
                if parts.len() == 2 && parts[0] == "session_id" {
                    Some(parts[1])
                } else {
                    None
                }
            })
            .next();
        
        if let Some(sid) = session_id {
            authenticate_session(&state, sid).await
        } else {
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
async fn authenticate_session(state: &AuthState, token: &str) -> Result<DbUser, StatusCode> {
    // Check database sessions if available
    if let Some(ref db) = state.database {
        match db.sessions.validate(token).await {
            Ok(Some(session)) => {
                // Get user from database
                match db.users.get_by_id(session.user_id).await {
                    Ok(Some(user)) => {
                        debug!("Authenticated user {} via session", user.email);
                        return Ok(user);
                    }
                    Ok(None) => {
                        error!("Session references non-existent user: {}", session.user_id);
                        return Err(StatusCode::UNAUTHORIZED);
                    }
                    Err(e) => {
                        error!("Failed to get user for session: {}", e);
                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                }
            }
            Ok(None) => {
                // Session not found in database, try OAuth manager
            }
            Err(e) => {
                error!("Failed to validate session: {}", e);
                // Continue to try OAuth manager
            }
        }
    }
    
    // Fallback to OAuth manager (for backward compatibility)
    match state.oauth_manager.get_session(token).await {
        Ok(Some(oauth_user)) => {
            // Create a DbUser from OAuth user
            // This is a temporary user object for backward compatibility
            let email = oauth_user.email.clone();
            Ok(DbUser {
                id: Uuid::parse_str(&oauth_user.id).unwrap_or_else(|_| Uuid::new_v4()),
                email: email.clone(),
                username: email.split('@').next().unwrap_or("user").to_string(),
                display_name: None,
                avatar_url: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                last_login_at: Some(chrono::Utc::now()),
                is_active: true,
                auth_provider: oauth_user.provider,
                provider_user_id: oauth_user.id,
            })
        }
        Ok(None) => {
            debug!("Invalid or expired session token");
            Err(StatusCode::UNAUTHORIZED)
        }
        Err(e) => {
            error!("Failed to validate session: {}", e);
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

/// Authenticate using API key
async fn authenticate_api_key(state: &AuthState, api_key: &str) -> Result<DbUser, StatusCode> {
    // API keys only work with database
    let db = state.database.as_ref()
        .ok_or(StatusCode::UNAUTHORIZED)?;
    
    match db.api_keys.validate(api_key).await {
        Ok(Some(key)) => {
            // Get the organization to check if it's active
            match db.organizations.get_by_id(key.organization_id).await {
                Ok(Some(org)) if org.is_active => {
                    // Get the user who created the key
                    match db.users.get_by_id(key.created_by_user_id).await {
                        Ok(Some(user)) => {
                            debug!("Authenticated via API key: {} for org: {}", key.name, org.name);
                            Ok(user)
                        }
                        Ok(None) => {
                            error!("API key references non-existent user: {}", key.created_by_user_id);
                            Err(StatusCode::UNAUTHORIZED)
                        }
                        Err(e) => {
                            error!("Failed to get user for API key: {}", e);
                            Err(StatusCode::INTERNAL_SERVER_ERROR)
                        }
                    }
                }
                Ok(Some(_)) => {
                    debug!("API key belongs to inactive organization");
                    Err(StatusCode::UNAUTHORIZED)
                }
                Ok(None) => {
                    error!("API key references non-existent organization: {}", key.organization_id);
                    Err(StatusCode::UNAUTHORIZED)
                }
                Err(e) => {
                    error!("Failed to get organization for API key: {}", e);
                    Err(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
        Ok(None) => {
            debug!("Invalid or expired API key");
            Err(StatusCode::UNAUTHORIZED)
        }
        Err(e) => {
            error!("Failed to validate API key: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// State for authentication middleware
#[derive(Clone)]
pub struct AuthState {
    pub oauth_manager: Arc<OAuthManager>,
    pub database: Option<Arc<Database>>,
}

impl AuthState {
    pub fn new(oauth_manager: Arc<OAuthManager>, database: Option<Arc<Database>>) -> Self {
        Self {
            oauth_manager,
            database,
        }
    }
}

/// Simplified auth middleware for backward compatibility
pub async fn auth_middleware_simple(
    State(sessions): State<Arc<RwLock<HashMap<String, domain::auth::types::AuthSession>>>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Extract session ID from cookie or header
    let session_id = if let Some(cookie_str) = 
        request.headers()
            .get("cookie")
            .and_then(|h| h.to_str().ok()) {
        
        // Parse cookies manually
        cookie_str
            .split(';')
            .filter_map(|c| {
                let parts: Vec<&str> = c.trim().splitn(2, '=').collect();
                if parts.len() == 2 && parts[0] == "session_id" {
                    Some(parts[1].to_string())
                } else {
                    None
                }
            })
            .next()
    } else if let Some(auth_header) = request.headers().get("authorization") {
        auth_header
            .to_str()
            .ok()
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(|s| s.to_string())
    } else {
        None
    };
    
    // Validate session
    match session_id {
        Some(sid) => {
            let sessions_guard = sessions.read().await;
            match sessions_guard.get(&sid) {
                Some(session) => {
                    // Check if session is expired (24 hours)
                    let now = chrono::Utc::now().timestamp();
                    if now - session.created_at > 86400 {
                        debug!("Session expired: {}", sid);
                        return Err(StatusCode::UNAUTHORIZED);
                    }
                    
                    // Create a temporary DbUser for compatibility
                    let user = DbUser {
                        id: Uuid::parse_str(&session.user.id).unwrap_or_else(|_| Uuid::new_v4()),
                        email: session.user.email.clone(),
                        username: session.user.email.split('@').next().unwrap_or("user").to_string(),
                        display_name: None,
                        avatar_url: None,
                        created_at: chrono::Utc::now(),
                        updated_at: chrono::Utc::now(),
                        last_login_at: Some(chrono::Utc::now()),
                        is_active: true,
                        auth_provider: session.user.provider.clone(),
                        provider_user_id: session.user.id.clone(),
                    };
                    
                    let mut request = request;
                    request.extensions_mut().insert(AuthenticatedUser(user));
                    Ok(next.run(request).await)
                }
                None => {
                    debug!("Invalid session ID: {}", sid);
                    Err(StatusCode::UNAUTHORIZED)
                }
            }
        }
        None => {
            debug!("No session ID provided");
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}