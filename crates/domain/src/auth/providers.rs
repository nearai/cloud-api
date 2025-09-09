// Simple OAuth2 Provider Implementations
//
// This module provides OAuth2 authentication for GitHub and Google.
// Only retrieves user email addresses.

use super::types::{AuthError, User, AuthSession};
use config::{GitHubOAuthConfig, GoogleOAuthConfig};
use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, 
    PkceCodeChallenge, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{debug, info};
use uuid::Uuid;

/// In-memory session storage (replace with Redis/DB in production)
pub type SessionStore = Arc<RwLock<HashMap<String, AuthSession>>>;

/// OAuth2 authentication manager
pub struct OAuthManager {
    github_client: Option<BasicClient>,
    google_client: Option<BasicClient>,
    pub sessions: SessionStore,
    http_client: Client,
}

impl OAuthManager {
    pub fn new(
        github_config: Option<GitHubOAuthConfig>,
        google_config: Option<GoogleOAuthConfig>,
    ) -> Result<Self, AuthError> {
        let github_client = if let Some(config) = github_config {
            Some(Self::create_github_client(config)?)
        } else {
            None
        };

        let google_client = if let Some(config) = google_config {
            Some(Self::create_google_client(config)?)
        } else {
            None
        };

        Ok(Self {
            github_client,
            google_client,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            http_client: Client::new(),
        })
    }

    fn create_github_client(config: GitHubOAuthConfig) -> Result<BasicClient, AuthError> {
        let auth_url = AuthUrl::new("https://github.com/login/oauth/authorize".to_string())
            .map_err(|e| AuthError::ConfigError(format!("Invalid GitHub auth URL: {}", e)))?;
        
        let token_url = TokenUrl::new("https://github.com/login/oauth/access_token".to_string())
            .map_err(|e| AuthError::ConfigError(format!("Invalid GitHub token URL: {}", e)))?;

        let client = BasicClient::new(
            ClientId::new(config.client_id),
            Some(ClientSecret::new(config.client_secret)),
            auth_url,
            Some(token_url),
        )
        .set_redirect_uri(
            RedirectUrl::new(config.redirect_url)
                .map_err(|e| AuthError::ConfigError(format!("Invalid redirect URL: {}", e)))?,
        );

        Ok(client)
    }

    fn create_google_client(config: GoogleOAuthConfig) -> Result<BasicClient, AuthError> {
        let auth_url = AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".to_string())
            .map_err(|e| AuthError::ConfigError(format!("Invalid Google auth URL: {}", e)))?;
        
        let token_url = TokenUrl::new("https://www.googleapis.com/oauth2/v3/token".to_string())
            .map_err(|e| AuthError::ConfigError(format!("Invalid Google token URL: {}", e)))?;

        let client = BasicClient::new(
            ClientId::new(config.client_id),
            Some(ClientSecret::new(config.client_secret)),
            auth_url,
            Some(token_url),
        )
        .set_redirect_uri(
            RedirectUrl::new(config.redirect_url)
                .map_err(|e| AuthError::ConfigError(format!("Invalid redirect URL: {}", e)))?,
        );

        Ok(client)
    }

    /// Generate authorization URL for GitHub
    pub fn github_auth_url(&self) -> Result<(String, String), AuthError> {
        let client = self.github_client.as_ref()
            .ok_or_else(|| AuthError::ConfigError("GitHub OAuth not configured".to_string()))?;

        let (auth_url, csrf_state) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("user:email".to_string()))
            .url();

        Ok((auth_url.to_string(), csrf_state.secret().to_string()))
    }

    /// Generate authorization URL for Google with PKCE
    pub fn google_auth_url(&self) -> Result<(String, String, String), AuthError> {
        let client = self.google_client.as_ref()
            .ok_or_else(|| AuthError::ConfigError("Google OAuth not configured".to_string()))?;

        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        
        let (auth_url, csrf_state) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("email".to_string()))
            .add_scope(Scope::new("openid".to_string()))
            .set_pkce_challenge(pkce_challenge)
            .url();

        Ok((auth_url.to_string(), csrf_state.secret().to_string(), pkce_verifier.secret().to_string()))
    }

    /// Handle GitHub OAuth callback
    pub async fn handle_github_callback(
        &self,
        code: String,
        _state: String,
    ) -> Result<String, AuthError> {
        let client = self.github_client.as_ref()
            .ok_or_else(|| AuthError::ConfigError("GitHub OAuth not configured".to_string()))?;

        debug!("Exchanging GitHub code for token");
        
        // Exchange code for token
        let token = client
            .exchange_code(AuthorizationCode::new(code))
            .request_async(async_http_client)
            .await
            .map_err(|e| AuthError::OAuthError(format!("Token exchange failed: {}", e)))?;

        let access_token = token.access_token().secret();
        
        // Get user info from GitHub
        let user_info = self.fetch_github_user(access_token).await?;
        
        // Create session
        let session_id = Uuid::new_v4().to_string();
        let session = AuthSession {
            session_id: session_id.clone(),
            user: User {
                id: user_info.id.to_string(),
                email: user_info.email.ok_or_else(|| {
                    AuthError::AuthFailed("GitHub user has no public email".to_string())
                })?,
                provider: "github".to_string(),
            },
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
            access_token: access_token.to_string(),
        };

        // Store session
        let mut sessions = self.sessions.write().await;
        sessions.insert(session_id.clone(), session);
        
        info!("GitHub user authenticated: {}", session_id);
        Ok(session_id)
    }

    /// Handle Google OAuth callback
    pub async fn handle_google_callback(
        &self,
        code: String,
        _state: String,
        pkce_verifier_str: String,
    ) -> Result<String, AuthError> {
        let client = self.google_client.as_ref()
            .ok_or_else(|| AuthError::ConfigError("Google OAuth not configured".to_string()))?;

        debug!("Exchanging Google code for token");
        
        // Convert verifier string back to PkceCodeVerifier
        let pkce_verifier = oauth2::PkceCodeVerifier::new(pkce_verifier_str);
        
        // Exchange code for token with PKCE
        let token = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(pkce_verifier)
            .request_async(async_http_client)
            .await
            .map_err(|e| AuthError::OAuthError(format!("Token exchange failed: {}", e)))?;

        let access_token = token.access_token().secret();
        
        // Get user info from Google
        let user_info = self.fetch_google_user(access_token).await?;
        
        // Create session
        let session_id = Uuid::new_v4().to_string();
        let session = AuthSession {
            session_id: session_id.clone(),
            user: User {
                id: user_info.id,
                email: user_info.email,
                provider: "google".to_string(),
            },
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
            access_token: access_token.to_string(),
        };

        // Store session
        let mut sessions = self.sessions.write().await;
        sessions.insert(session_id.clone(), session);
        
        info!("Google user authenticated: {}", session_id);
        Ok(session_id)
    }

    /// Fetch GitHub user information
    async fn fetch_github_user(&self, access_token: &str) -> Result<GitHubUser, AuthError> {
        let response = self.http_client
            .get("https://api.github.com/user")
            .header("Authorization", format!("Bearer {}", access_token))
            .header("User-Agent", "platform-api")
            .send()
            .await
            .map_err(|e| AuthError::NetworkError(format!("Failed to fetch GitHub user: {}", e)))?;

        if !response.status().is_success() {
            return Err(AuthError::AuthFailed(format!(
                "GitHub API returned status: {}",
                response.status()
            )));
        }

        let mut user: GitHubUser = response.json().await
            .map_err(|e| AuthError::AuthFailed(format!("Failed to parse GitHub user: {}", e)))?;

        // If no public email, fetch from emails endpoint
        if user.email.is_none() {
            let emails_response = self.http_client
                .get("https://api.github.com/user/emails")
                .header("Authorization", format!("Bearer {}", access_token))
                .header("User-Agent", "platform-api")
                .send()
                .await
                .map_err(|e| AuthError::NetworkError(format!("Failed to fetch GitHub emails: {}", e)))?;

            if emails_response.status().is_success() {
                let emails: Vec<GitHubEmail> = emails_response.json().await
                    .map_err(|e| AuthError::AuthFailed(format!("Failed to parse GitHub emails: {}", e)))?;
                
                // Get primary email
                if let Some(primary) = emails.iter().find(|e| e.primary) {
                    user.email = Some(primary.email.clone());
                } else if let Some(first) = emails.first() {
                    user.email = Some(first.email.clone());
                }
            }
        }

        Ok(user)
    }

    /// Fetch Google user information
    async fn fetch_google_user(&self, access_token: &str) -> Result<GoogleUser, AuthError> {
        debug!("Fetching Google user info with access token");
        
        let response = self.http_client
            .get("https://www.googleapis.com/oauth2/v2/userinfo")
            .header("Authorization", format!("Bearer {}", access_token))
            .send()
            .await
            .map_err(|e| AuthError::NetworkError(format!("Failed to fetch Google user: {}", e)))?;

        let status = response.status();
        debug!("Google API response status: {}", status);
        
        // Get the response text
        let response_text = response.text().await
            .map_err(|e| AuthError::NetworkError(format!("Failed to read response: {}", e)))?;
        
        debug!("Google API response body: {}", response_text);
        
        if !status.is_success() {
            return Err(AuthError::AuthFailed(format!(
                "Google API returned status: {}, body: {}",
                status, response_text
            )));
        }
        
        // Try to parse the JSON
        serde_json::from_str::<GoogleUser>(&response_text)
            .map_err(|e| AuthError::AuthFailed(format!("Failed to parse Google user: {}. Response was: {}", e, response_text)))
    }

    /// Get session by ID
    pub async fn get_session(&self, session_id: &str) -> Option<AuthSession> {
        let sessions = self.sessions.read().await;
        sessions.get(session_id).cloned()
    }

    /// Remove session
    pub async fn logout(&self, session_id: &str) -> bool {
        let mut sessions = self.sessions.write().await;
        sessions.remove(session_id).is_some()
    }

    /// Clean up expired sessions (older than 24 hours)
    pub async fn cleanup_sessions(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        
        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, session| {
            now - session.created_at < 86400 // 24 hours
        });
    }
}

#[derive(Deserialize)]
struct GitHubUser {
    id: u64,
    email: Option<String>,
}

#[derive(Deserialize)]
struct GitHubEmail {
    email: String,
    primary: bool,
}

#[derive(Debug, Deserialize)]
struct GoogleUser {
    #[serde(alias = "sub", alias = "id")]
    id: String,
    email: String,
    #[serde(default)]
    verified_email: Option<bool>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    picture: Option<String>,
}