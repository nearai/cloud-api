// OAuth2 Authentication Routes
//
// Handles GitHub and Google OAuth2 login flows

use axum::{
    extract::{Query, State},
    http::{header::SET_COOKIE, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Extension, Json,
};
use crate::middleware::AuthenticatedUser;
use domain::auth::OAuthManager;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

/// Temporary storage for OAuth state and PKCE verifiers
/// In production, use Redis or similar
pub type StateStore = Arc<RwLock<HashMap<String, OAuthState>>>;

#[derive(Clone)]
pub struct OAuthState {
    provider: String,
    pkce_verifier: Option<String>,  // Store verifier as string
}

#[derive(Deserialize)]
pub struct OAuthCallback {
    code: String,
    state: String,
}

#[derive(Serialize)]
pub struct AuthResponse {
    message: String,
    email: String,
    provider: String,
}

/// Initiate GitHub OAuth flow - redirects to GitHub
pub async fn github_login(
    State((oauth, state_store)): State<(Arc<OAuthManager>, StateStore)>,
) -> Result<Redirect, StatusCode> {
    debug!("Initiating GitHub OAuth flow");
    
    let (auth_url, state) = oauth.github_auth_url()
        .map_err(|e| {
            error!("Failed to generate GitHub auth URL: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    
    // Store state for verification
    let mut store = state_store.write().await;
    store.insert(state.clone(), OAuthState {
        provider: "github".to_string(),
        pkce_verifier: None,
    });
    
    info!("Redirecting to GitHub with state: {}", state);
    Ok(Redirect::to(&auth_url))
}

/// Initiate Google OAuth flow - redirects to Google
pub async fn google_login(
    State((oauth, state_store)): State<(Arc<OAuthManager>, StateStore)>,
) -> Result<Redirect, StatusCode> {
    debug!("Initiating Google OAuth flow");
    
    let (auth_url, state, pkce_verifier) = oauth.google_auth_url()
        .map_err(|e| {
            error!("Failed to generate Google auth URL: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    
    // Store state and PKCE verifier for verification
    let mut store = state_store.write().await;
    store.insert(state.clone(), OAuthState {
        provider: "google".to_string(),
        pkce_verifier: Some(pkce_verifier),
    });
    
    info!("Redirecting to Google with state: {}", state);
    Ok(Redirect::to(&auth_url))
}

/// Handle OAuth callback from both providers
pub async fn oauth_callback(
    Query(params): Query<OAuthCallback>,
    State((oauth, state_store)): State<(Arc<OAuthManager>, StateStore)>,
) -> Response {
    debug!("OAuth callback received with state: {}", params.state);
    
    // Retrieve and verify state
    let oauth_state = {
        let mut store = state_store.write().await;
        store.remove(&params.state)
    };
    
    let oauth_state = match oauth_state {
        Some(state) => state,
        None => {
            error!("Invalid or expired OAuth state: {}", params.state);
            return (StatusCode::BAD_REQUEST, "Invalid state parameter").into_response();
        }
    };
    
    info!("Processing {} OAuth callback", oauth_state.provider);
    
    // Handle provider-specific callback
    let session_id = match oauth_state.provider.as_str() {
        "github" => {
            oauth.handle_github_callback(params.code, params.state).await
        }
        "google" => {
            let verifier = oauth_state.pkce_verifier.unwrap();
            oauth.handle_google_callback(params.code, params.state, verifier).await
        }
        _ => {
            error!("Unknown provider: {}", oauth_state.provider);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Unknown provider").into_response();
        }
    };
    
    match session_id {
        Ok(session_id) => {
            info!("OAuth authentication successful, session: {}", session_id);
            
            // Set session cookie and redirect to success page
            let cookie = format!(
                "session_id={}; HttpOnly; SameSite=Lax; Path=/; Max-Age=86400",
                session_id
            );
            
            (
                StatusCode::SEE_OTHER,
                [(SET_COOKIE, cookie)],
                Redirect::to("/v1/auth/success"),
            ).into_response()
        }
        Err(e) => {
            error!("OAuth authentication failed: {}", e);
            (
                StatusCode::UNAUTHORIZED,
                format!("Authentication failed: {}", e),
            ).into_response()
        }
    }
}

/// Get current user information
pub async fn current_user(
    Extension(user): Extension<AuthenticatedUser>,
) -> Json<AuthResponse> {
    Json(AuthResponse {
        message: "Authenticated".to_string(),
        email: user.0.email.clone(),
        provider: user.0.auth_provider.clone(),
    })
}

/// Logout endpoint
pub async fn logout(
    Extension(user): Extension<AuthenticatedUser>,
) -> Response {
    debug!("Logging out user: {}", user.0.email);
    
    // This would need the session_id from cookie in real implementation
    // For now, just clear the cookie
    let cookie = "session_id=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0";
    
    (
        StatusCode::OK,
        [(SET_COOKIE, cookie)],
        Json(serde_json::json!({
            "message": "Logged out successfully"
        })),
    ).into_response()
}

/// Success page after authentication
pub async fn auth_success() -> Html<&'static str> {
    Html(r#"<!DOCTYPE html>
<html>
<head>
    <title>Authentication Successful</title>
    <style>
        body { 
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
            display: flex;
            justify-content: center;
            align-items: center;
            height: 100vh;
            margin: 0;
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
        }
        .container {
            background: white;
            padding: 2rem;
            border-radius: 8px;
            box-shadow: 0 4px 6px rgba(0,0,0,0.1);
            text-align: center;
        }
        h1 { color: #2d3748; }
        p { color: #4a5568; margin: 1rem 0; }
        .success { color: #48bb78; font-weight: bold; }
    </style>
</head>
<body>
    <div class="container">
        <h1>🎉 Authentication Successful!</h1>
        <p class="success">You are now logged in.</p>
        <p>You can close this window and return to your application.</p>
    </div>
</body>
</html>"#)
}

/// Login page with OAuth provider options
pub async fn login_page() -> Html<&'static str> {
    Html(r##"<!DOCTYPE html>
<html>
<head>
    <title>Login - Platform API</title>
    <style>
        body { 
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
            display: flex;
            justify-content: center;
            align-items: center;
            height: 100vh;
            margin: 0;
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
        }
        .container {
            background: white;
            padding: 3rem;
            border-radius: 12px;
            box-shadow: 0 10px 25px rgba(0,0,0,0.1);
            text-align: center;
            max-width: 400px;
            width: 100%;
        }
        h1 { 
            color: #2d3748; 
            margin-bottom: 0.5rem;
        }
        p { 
            color: #718096; 
            margin-bottom: 2rem;
        }
        .login-btn {
            display: flex;
            align-items: center;
            justify-content: center;
            width: 100%;
            padding: 0.75rem 1rem;
            margin: 0.75rem 0;
            border: 1px solid #e2e8f0;
            border-radius: 8px;
            background: white;
            color: #2d3748;
            text-decoration: none;
            font-size: 1rem;
            font-weight: 500;
            transition: all 0.2s;
            cursor: pointer;
        }
        .login-btn:hover {
            background: #f7fafc;
            transform: translateY(-2px);
            box-shadow: 0 4px 12px rgba(0,0,0,0.1);
        }
        .login-btn svg {
            width: 20px;
            height: 20px;
            margin-right: 12px;
        }
        .github-btn:hover {
            border-color: #24292e;
        }
        .google-btn:hover {
            border-color: #4285f4;
        }
        .divider {
            margin: 2rem 0;
            color: #a0aec0;
            font-size: 0.875rem;
        }
    </style>
</head>
<body>
    <div class="container">
        <h1>Welcome</h1>
        <p>Sign in to access the Platform API</p>
        
        <a href="/v1/auth/github" class="login-btn github-btn">
            <svg viewBox="0 0 24 24" fill="currentColor">
                <path d="M12 2C6.477 2 2 6.477 2 12c0 4.42 2.865 8.17 6.839 9.49.5.092.682-.217.682-.482 0-.237-.008-.866-.013-1.7-2.782.603-3.369-1.34-3.369-1.34-.454-1.156-1.11-1.463-1.11-1.463-.908-.62.069-.608.069-.608 1.003.07 1.531 1.03 1.531 1.03.892 1.529 2.341 1.087 2.91.832.092-.647.35-1.088.636-1.338-2.22-.253-4.555-1.11-4.555-4.943 0-1.091.39-1.984 1.029-2.683-.103-.253-.446-1.27.098-2.647 0 0 .84-.269 2.75 1.025A9.578 9.578 0 0112 6.836c.85.004 1.705.114 2.504.336 1.909-1.294 2.747-1.025 2.747-1.025.546 1.377.203 2.394.1 2.647.64.699 1.028 1.592 1.028 2.683 0 3.842-2.339 4.687-4.566 4.935.359.309.678.919.678 1.852 0 1.336-.012 2.415-.012 2.743 0 .267.18.578.688.48C19.138 20.167 22 16.418 22 12c0-5.523-4.477-10-10-10z"/>
            </svg>
            Continue with GitHub
        </a>
        
        <a href="/v1/auth/google" class="login-btn google-btn">
            <svg viewBox="0 0 24 24">
                <path fill="#4285F4" d="M22.56 12.25c0-.78-.07-1.53-.2-2.25H12v4.26h5.92c-.26 1.37-1.04 2.53-2.21 3.31v2.77h3.57c2.08-1.92 3.28-4.74 3.28-8.09z"/>
                <path fill="#34A853" d="M12 23c2.97 0 5.46-.98 7.28-2.66l-3.57-2.77c-.98.66-2.23 1.06-3.71 1.06-2.86 0-5.29-1.93-6.16-4.53H2.18v2.84C3.99 20.53 7.7 23 12 23z"/>
                <path fill="#FBBC05" d="M5.84 14.09c-.22-.66-.35-1.36-.35-2.09s.13-1.43.35-2.09V7.07H2.18C1.43 8.55 1 10.22 1 12s.43 3.45 1.18 4.93l2.85-2.22.81-.62z"/>
                <path fill="#EA4335" d="M12 5.38c1.62 0 3.06.56 4.21 1.64l3.15-3.15C17.45 2.09 14.97 1 12 1 7.7 1 3.99 3.47 2.18 7.07l3.66 2.84c.87-2.6 3.3-4.53 6.16-4.53z"/>
            </svg>
            Continue with Google
        </a>
        
        <div class="divider">Secure OAuth 2.0 Authentication</div>
    </div>
</body>
</html>"##)
}