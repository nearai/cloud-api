// Simple Session-based Authentication Middleware
//
// This module provides Axum middleware for session-based authentication
// using OAuth2 providers (GitHub and Google).

use axum::{
    extract::{Request, State},
    http::{header::COOKIE, StatusCode},
    middleware::Next,
    response::Response,
};
use domain::auth::{SessionStore, User};
use tracing::{debug, warn};

/// User information extracted from authentication
/// This is added to request extensions after successful authentication
#[derive(Debug, Clone)]
pub struct AuthenticatedUser(pub User);

/// Authentication middleware that validates session cookies
/// 
/// This middleware:
/// 1. Extracts session ID from cookie
/// 2. Validates session exists in store
/// 3. Adds user information to request extensions
/// 4. Passes request to next handler if valid, returns 401 if invalid
pub async fn auth_middleware(
    State(sessions): State<SessionStore>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let uri = request.uri().path();
    let method = request.method().as_str();
    debug!("Auth middleware processing: {} {}", method, uri);
    
    // Extract session ID from cookie
    let session_id = extract_session_from_request(&request)?;
    debug!("Extracted session ID: {}", session_id);
    
    // Validate session
    if let Some(session) = sessions.read().await.get(&session_id) {
        debug!("Session found for user: {} ({})", session.user.email, session.user.provider);
        // Add user to request extensions so route handlers can access it
        request.extensions_mut().insert(AuthenticatedUser(session.user.clone()));
        // Continue to the next handler
        Ok(next.run(request).await)
    } else {
        warn!("Session not found: {}", session_id);
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Extract session ID from cookie
fn extract_session_from_request(request: &Request) -> Result<String, StatusCode> {
    debug!("Extracting session from cookie");
    
    let cookies = request
        .headers()
        .get(COOKIE)
        .ok_or_else(|| {
            debug!("No Cookie header found in request");
            StatusCode::UNAUTHORIZED
        })?
        .to_str()
        .map_err(|_| {
            debug!("Cookie header contains invalid UTF-8");
            StatusCode::BAD_REQUEST
        })?;

    // Parse cookies and find session_id
    for cookie in cookies.split(';') {
        let cookie = cookie.trim();
        if let Some(value) = cookie.strip_prefix("session_id=") {
            debug!("Found session_id cookie");
            return Ok(value.to_string());
        }
    }

    debug!("No session_id cookie found");
    Err(StatusCode::UNAUTHORIZED)
}

/// Optional middleware that allows unauthenticated requests but extracts user if session present
/// Useful for endpoints that have different behavior for authenticated vs anonymous users
pub async fn optional_auth_middleware(
    State(sessions): State<SessionStore>,
    mut request: Request,
    next: Next,
) -> Response {
    // Try to extract session, but don't fail if not present
    if let Ok(session_id) = extract_session_from_request(&request) {
        // Try to validate session, but don't fail if invalid
        if let Some(session) = sessions.read().await.get(&session_id) {
            debug!("Optional auth: authenticated user {}", session.user.email);
            request.extensions_mut().insert(AuthenticatedUser(session.user.clone()));
        } else {
            debug!("Optional auth: invalid session provided");
        }
    } else {
        debug!("Optional auth: no session provided");
    }
    
    next.run(request).await
}