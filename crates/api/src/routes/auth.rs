use crate::middleware::AuthenticatedUser;
use axum::{
    extract::{Query, Request, State},
    http::{header::SET_COOKIE, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Extension, Json,
};
use chrono::Utc;
use config::ApiConfig;
use database::repositories::OAuthStateRepository;
use serde::{Deserialize, Serialize};
use services::auth::{AuthServiceTrait, OAuthManager};
use std::sync::Arc;
use tracing::{debug, error, info};

/// OAuth state storage backed by PostgreSQL for multi-instance support
pub type StateStore = Arc<OAuthStateRepository>;

pub type AuthState = (
    Arc<OAuthManager>,
    StateStore,
    Arc<dyn AuthServiceTrait>,
    Arc<ApiConfig>,
);

pub type NearAuthState = (Arc<services::auth::NearAuthService>, Arc<ApiConfig>);

#[derive(Deserialize)]
pub struct OAuthCallback {
    code: String,
    state: String,
}

#[derive(Serialize)]
pub struct TokenExchangeResponse {
    access_token: String,
    refresh_token: String,
    refresh_token_expiration: chrono::DateTime<Utc>,
    user: AuthResponse,
}

#[derive(Serialize)]
pub struct AuthResponse {
    message: String,
    email: String,
    provider: String,
}

// NEAR authentication types
use base64::prelude::*;
use near_api::signer::NEP413Payload;

/// Request body for NEAR authentication (NEP-413)
#[derive(Debug, Deserialize)]
pub struct NearAuthRequest {
    /// The signed message from the wallet
    pub signed_message: NearSignedMessageJson,
    /// The payload that was signed
    pub payload: NearPayloadJson,
}

/// Signed message from wallet (NEP-413 SignedMessage)
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NearSignedMessageJson {
    /// NEAR account ID (e.g., "alice.near")
    pub account_id: String,
    /// Public key used to sign (e.g., "ed25519:...")
    pub public_key: String,
    /// Base64-encoded signature
    pub signature: String,
    /// Optional state for browser wallets
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// Payload that was signed (NEP-413 Payload)
#[derive(Debug)]
pub struct NearPayloadJson {
    /// The message that was signed
    pub message: String,
    /// The nonce (as array of 32 bytes)
    pub nonce: Vec<u8>,
    /// The recipient (your app identifier)
    pub recipient: String,
    /// Optional callback URL
    pub callback_url: Option<String>,
}

impl TryFrom<NearSignedMessageJson> for services::auth::near::SignedMessage {
    type Error = anyhow::Error;

    fn try_from(msg: NearSignedMessageJson) -> Result<Self, Self::Error> {
        use near_api::types::Signature;

        let public_key: near_api::PublicKey = msg.public_key.parse()?;

        // Parse base64 signature and create Signature based on key type
        let sig_bytes = BASE64_STANDARD.decode(&msg.signature)?;
        let signature = Signature::from_parts(public_key.key_type(), &sig_bytes)?;

        Ok(services::auth::near::SignedMessage {
            account_id: msg.account_id.parse()?,
            public_key,
            signature,
            state: msg.state,
        })
    }
}

impl TryFrom<NearPayloadJson> for NEP413Payload {
    type Error = anyhow::Error;

    fn try_from(payload: NearPayloadJson) -> Result<Self, Self::Error> {
        let nonce: [u8; 32] = payload
            .nonce
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("Nonce must be exactly 32 bytes"))?;

        Ok(NEP413Payload {
            message: payload.message,
            nonce,
            recipient: payload.recipient,
            callback_url: payload.callback_url,
        })
    }
}

// Custom deserializer for NearPayloadJson to validate nonce length
impl<'de> serde::Deserialize<'de> for NearPayloadJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct NearPayloadJsonRaw {
            message: String,
            nonce: Vec<u8>,
            recipient: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            callback_url: Option<String>,
        }

        let raw = NearPayloadJsonRaw::deserialize(deserializer)?;

        // Validate nonce length during deserialization
        if raw.nonce.len() != 32 {
            return Err(serde::de::Error::custom("Nonce must be exactly 32 bytes"));
        }

        Ok(NearPayloadJson {
            message: raw.message,
            nonce: raw.nonce,
            recipient: raw.recipient,
            callback_url: raw.callback_url,
        })
    }
}

#[derive(Serialize)]
pub struct NearAuthResponse {
    access_token: String,
    refresh_token: String,
    refresh_token_expiration: chrono::DateTime<Utc>,
    user: AuthResponse,
    is_new_user: bool,
}

/// Initiate GitHub OAuth flow - redirects to GitHub
pub async fn github_login(
    State((oauth, state_store, _auth_service, _config)): State<AuthState>,
) -> Result<Redirect, StatusCode> {
    debug!("Initiating GitHub OAuth flow");

    let (auth_url, state) = oauth.github_auth_url().map_err(|_| {
        error!("Failed to generate GitHub auth URL");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Store state in database for multi-instance support
    state_store
        .create(state.clone(), "github".to_string(), None)
        .await
        .map_err(|e| {
            error!("Failed to store OAuth state: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    debug!("Redirecting to GitHub with state: {}", state);
    Ok(Redirect::to(&auth_url))
}

/// Initiate Google OAuth flow - redirects to Google
pub async fn google_login(
    State((oauth, state_store, _auth_service, _config)): State<AuthState>,
) -> Result<Redirect, StatusCode> {
    debug!("Initiating Google OAuth flow");

    let (auth_url, state, pkce_verifier) = oauth.google_auth_url().map_err(|_| {
        error!("Failed to generate Google auth URL");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Store state and PKCE verifier in database for multi-instance support
    state_store
        .create(state.clone(), "google".to_string(), Some(pkce_verifier))
        .await
        .map_err(|e| {
            error!("Failed to store OAuth state: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    debug!("Redirecting to Google with state: {}", state);
    Ok(Redirect::to(&auth_url))
}

/// Handle OAuth callback - NEW frontend-centric flow
///
/// Frontend calls this endpoint with the OAuth code to exchange for session token.
/// Can be called via:
/// - GET /v1/auth/callback?code=xxx&state=yyy (direct OAuth callback)  
/// - POST /v1/auth/callback with JSON body (explicit token exchange)
pub async fn oauth_callback(
    Query(params): Query<OAuthCallback>,
    State((oauth, state_store, auth_service, config)): State<AuthState>,
    request: Request,
) -> Response {
    debug!("OAuth callback received with state: {}", params.state);

    let user_agent_header = match request
        .headers()
        .get("User-Agent")
        .and_then(|h| h.to_str().ok())
    {
        Some(ua) => ua,
        None => {
            error!("Missing User-Agent header in OAuth callback");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "bad_request",
                    "error_description": "Missing User-Agent header"
                })),
            )
                .into_response();
        }
    };

    // Retrieve and delete state from database (atomic operation prevents replay attacks)
    let oauth_state_row = match state_store.get_and_delete(&params.state).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            error!("Invalid or expired OAuth state: {}", params.state);
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_request",
                    "error_description": "Invalid or expired state parameter"
                })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Database error retrieving OAuth state: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "internal_server_error",
                    "error_description": "Failed to verify OAuth state"
                })),
            )
                .into_response();
        }
    };

    info!("Processing {} OAuth callback", oauth_state_row.provider);

    // Handle provider-specific callback to get OAuth user info
    let oauth_result = match oauth_state_row.provider.as_str() {
        "github" => {
            oauth
                .handle_github_callback(params.code, params.state)
                .await
        }
        "google" => {
            let verifier = match oauth_state_row.pkce_verifier {
                Some(v) => v,
                None => {
                    error!("Missing PKCE verifier for Google OAuth");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": "internal_server_error",
                            "error_description": "Missing PKCE verifier for Google OAuth"
                        })),
                    )
                        .into_response();
                }
            };
            oauth
                .handle_google_callback(params.code, params.state, verifier)
                .await
        }
        _ => {
            error!("Unknown provider: {}", oauth_state_row.provider);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "internal_server_error",
                    "error_description": "Unknown OAuth provider"
                })),
            )
                .into_response();
        }
    };

    let (oauth_user_info, _access_token) = match oauth_result {
        Ok(result) => result,
        Err(e) => {
            error!("OAuth authentication failed");
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "access_denied",
                    "error_description": format!("OAuth authentication failed: {}", e)
                })),
            )
                .into_response();
        }
    };

    // Get or create user from OAuth info
    let user = match auth_service.get_or_create_oauth_user(oauth_user_info).await {
        Ok(user) => user,
        Err(e) => {
            error!("Failed to get or create user");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "internal_server_error",
                    "error_description": format!("User creation failed: {}", e)
                })),
            )
                .into_response();
        }
    };

    // Create session for the user (24 hours)
    let session_result = auth_service
        .create_session(
            user.id,
            None,
            user_agent_header.to_string(),
            config.auth.encoding_key.to_string(),
            1,
            7 * 24,
        )
        .await;

    match session_result {
        Ok((access_token, refresh_session, refresh_token)) => {
            debug!("Session created successfully for user: {}", user.email);

            let response = TokenExchangeResponse {
                access_token,
                refresh_token,
                refresh_token_expiration: refresh_session.expires_at,
                user: AuthResponse {
                    message: "Authenticated".to_string(),
                    email: user.email.clone(),
                    provider: oauth_state_row.provider,
                },
            };

            Json(response).into_response()
        }
        Err(e) => {
            error!("Failed to create session");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "internal_server_error",
                    "error_description": format!("Session creation failed: {}", e)
                })),
            )
                .into_response()
        }
    }
}

/// Get current user information
pub async fn current_user(Extension(user): Extension<AuthenticatedUser>) -> Json<AuthResponse> {
    Json(AuthResponse {
        message: "Authenticated".to_string(),
        email: user.0.email.clone(),
        provider: user.0.auth_provider.clone(),
    })
}

/// Logout endpoint
pub async fn logout(Extension(user): Extension<AuthenticatedUser>) -> Response {
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
    )
        .into_response()
}

/// NEAR wallet login endpoint
pub async fn near_login(
    State((near_auth_service, config)): State<NearAuthState>,
    headers: HeaderMap,
    Json(auth_request): Json<NearAuthRequest>,
) -> Response {
    let account_id_str = auth_request.signed_message.account_id.clone();
    debug!(
        "NEAR authentication attempt for account: {}",
        account_id_str
    );

    // Convert JSON types to internal types
    let signed_message = match auth_request.signed_message.try_into() {
        Ok(msg) => msg,
        Err(e) => {
            error!("Failed to parse signed message: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "bad_request",
                    "error_description": "Invalid signed message format"
                })),
            )
                .into_response();
        }
    };

    let payload = match auth_request.payload.try_into() {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to parse payload: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "bad_request",
                    "error_description": "Invalid payload format"
                })),
            )
                .into_response();
        }
    };

    // Extract user agent
    let user_agent_header = match headers.get("User-Agent").and_then(|h| h.to_str().ok()) {
        Some(ua) => ua,
        None => {
            error!("Missing User-Agent header");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "bad_request",
                    "error_description": "Missing User-Agent header"
                })),
            )
                .into_response();
        }
    };

    // Verify and authenticate
    let result = near_auth_service
        .verify_and_authenticate(
            signed_message,
            payload,
            None, // ip_address
            user_agent_header.to_string(),
            config.auth.encoding_key.clone(),
        )
        .await;

    match result {
        Ok((access_token, refresh_session, refresh_token, is_new_user)) => {
            debug!("NEAR authentication successful: {}", account_id_str);

            let response = NearAuthResponse {
                access_token,
                refresh_token,
                refresh_token_expiration: refresh_session.expires_at,
                user: AuthResponse {
                    message: if is_new_user {
                        "Account created"
                    } else {
                        "Authenticated"
                    }
                    .to_string(),
                    email: format!("{account_id_str}@near"),
                    provider: "near".to_string(),
                },
                is_new_user,
            };

            Json(response).into_response()
        }
        Err(e) => {
            error!("NEAR authentication failed: {}", e);
            let error_msg = e.to_string();
            let (status, error_type) = if error_msg.contains("Invalid signature")
                || error_msg.contains("replay attack")
                || error_msg.contains("expired")
            {
                (StatusCode::UNAUTHORIZED, "invalid_signature")
            } else if error_msg.contains("Invalid recipient")
                || error_msg.contains("Invalid message")
            {
                (StatusCode::BAD_REQUEST, "invalid_request")
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            };

            (
                status,
                Json(serde_json::json!({
                    "error": error_type,
                    "error_description": error_msg
                })),
            )
                .into_response()
        }
    }
}

/// Login page with OAuth provider options
pub async fn login_page() -> Html<&'static str> {
    Html(
        r##"<!DOCTYPE html>
<html>
<head>
    <title>Login - NEAR AI Cloud API</title>
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
        <p>Sign in to access the NEAR AI Cloud API</p>
        
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
</html>"##,
    )
}
