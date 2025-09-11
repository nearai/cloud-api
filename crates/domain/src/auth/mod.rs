pub mod providers;
pub mod types;

use crate::auth::types::AuthError;
use config::OAuthProviderConfig;
use database::Database;
use oauth2::{
    basic::BasicClient, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenUrl, TokenResponse,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};
use uuid::Uuid;

/// OAuth Manager handles authentication flows
pub struct OAuthManager {
    github_client: Option<BasicClient>,
    google_client: Option<BasicClient>,
    pub sessions: Arc<RwLock<HashMap<String, types::AuthSession>>>,
    database: Option<Arc<Database>>,
}

impl OAuthManager {
    /// Create a new OAuth manager
    pub fn new(
        github_config: Option<OAuthProviderConfig>,
        google_config: Option<OAuthProviderConfig>,
    ) -> Result<Self, AuthError> {
        let github_client = github_config
            .map(|config| Self::create_github_client(config))
            .transpose()?;

        let google_client = google_config
            .map(|config| Self::create_google_client(config))
            .transpose()?;

        Ok(Self {
            github_client,
            google_client,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            database: None,
        })
    }

    /// Set the database for the OAuth manager
    pub fn with_database(mut self, database: Arc<Database>) -> Self {
        self.database = Some(database);
        self
    }

    /// Create GitHub OAuth client
    fn create_github_client(config: OAuthProviderConfig) -> Result<BasicClient, AuthError> {
        let client = BasicClient::new(
            ClientId::new(config.client_id),
            Some(ClientSecret::new(config.client_secret)),
            AuthUrl::new("https://github.com/login/oauth/authorize".to_string())
                .map_err(|e| AuthError::ConfigError(e.to_string()))?,
            Some(
                TokenUrl::new("https://github.com/login/oauth/access_token".to_string())
                    .map_err(|e| AuthError::ConfigError(e.to_string()))?,
            ),
        )
        .set_redirect_uri(
            RedirectUrl::new(config.redirect_uri)
                .map_err(|e| AuthError::ConfigError(e.to_string()))?,
        );

        Ok(client)
    }

    /// Create Google OAuth client
    fn create_google_client(config: OAuthProviderConfig) -> Result<BasicClient, AuthError> {
        let client = BasicClient::new(
            ClientId::new(config.client_id),
            Some(ClientSecret::new(config.client_secret)),
            AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".to_string())
                .map_err(|e| AuthError::ConfigError(e.to_string()))?,
            Some(
                TokenUrl::new("https://oauth2.googleapis.com/token".to_string())
                    .map_err(|e| AuthError::ConfigError(e.to_string()))?,
            ),
        )
        .set_redirect_uri(
            RedirectUrl::new(config.redirect_uri)
                .map_err(|e| AuthError::ConfigError(e.to_string()))?,
        );

        Ok(client)
    }

    /// Generate GitHub authorization URL
    pub fn github_auth_url(&self) -> Result<(String, String), AuthError> {
        let client = self
            .github_client
            .as_ref()
            .ok_or_else(|| AuthError::ConfigError("GitHub OAuth not configured".to_string()))?;

        let (auth_url, csrf_token) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("user:email".to_string()))
            .url();

        Ok((auth_url.to_string(), csrf_token.secret().clone()))
    }

    /// Generate Google authorization URL with PKCE
    pub fn google_auth_url(&self) -> Result<(String, String, String), AuthError> {
        let client = self
            .google_client
            .as_ref()
            .ok_or_else(|| AuthError::ConfigError("Google OAuth not configured".to_string()))?;

        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let (auth_url, csrf_token) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("openid".to_string()))
            .add_scope(Scope::new("email".to_string()))
            .add_scope(Scope::new("profile".to_string()))
            .set_pkce_challenge(pkce_challenge)
            .url();

        Ok((
            auth_url.to_string(),
            csrf_token.secret().clone(),
            pkce_verifier.secret().clone(),
        ))
    }

    /// Handle GitHub OAuth callback
    pub async fn handle_github_callback(
        &self,
        code: String,
        _state: String,
    ) -> Result<String, AuthError> {
        debug!("Processing GitHub OAuth callback");

        let client = self
            .github_client
            .as_ref()
            .ok_or_else(|| AuthError::ConfigError("GitHub OAuth not configured".to_string()))?;

        // Exchange code for token
        let token_result = client
            .exchange_code(AuthorizationCode::new(code))
            .request_async(oauth2::reqwest::async_http_client)
            .await
            .map_err(|e| AuthError::OAuthError(e.to_string()))?;

        let access_token = token_result.access_token().secret();

        // Fetch user info from GitHub
        let user_info = providers::fetch_github_user(access_token).await?;

        // Store or update user in database if available
        if let Some(ref db) = self.database {
            // Ensure we have an email
            let email = user_info.email.clone()
                .ok_or_else(|| AuthError::AuthFailed("GitHub user has no email".to_string()))?;
            
            let user_service = crate::services::UserService::new(db.clone());
            let db_user = user_service.get_or_create_oauth_user(
                email.clone(),
                user_info.login.clone(),
                user_info.name.clone(),
                user_info.avatar_url.clone(),
                "github".to_string(),
                user_info.id.to_string(),
            ).await.map_err(|e| AuthError::AuthFailed(e.to_string()))?;

            // Update last login (already handled in user service)

            // Create a session
            let (_session, token) = db.sessions.create(
                db_user.id,
                None, // IP address would come from request context
                None, // User agent would come from request context
                24,   // 24 hours
            ).await.map_err(|e| AuthError::AuthFailed(e.to_string()))?;

            info!("User authenticated via GitHub: {}", email);
            return Ok(token);
        }

        // Fallback to in-memory session (backward compatibility)
        let email = user_info.email
            .ok_or_else(|| AuthError::AuthFailed("GitHub user has no email".to_string()))?;
        
        let session_id = Uuid::new_v4().to_string();
        let user = types::User {
            id: user_info.id.to_string(),
            email: email.clone(),
            provider: "github".to_string(),
        };

        let auth_session = types::AuthSession {
            session_id: session_id.clone(),
            user,
            created_at: chrono::Utc::now().timestamp(),
            access_token: access_token.to_string(),
        };

        let mut sessions = self.sessions.write().await;
        sessions.insert(session_id.clone(), auth_session);

        info!("User authenticated via GitHub: {}", email);
        Ok(session_id)
    }

    /// Handle Google OAuth callback
    pub async fn handle_google_callback(
        &self,
        code: String,
        _state: String,
        pkce_verifier: String,
    ) -> Result<String, AuthError> {
        debug!("Processing Google OAuth callback");

        let client = self
            .google_client
            .as_ref()
            .ok_or_else(|| AuthError::ConfigError("Google OAuth not configured".to_string()))?;

        // Exchange code for token with PKCE verifier
        let token_result = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier))
            .request_async(oauth2::reqwest::async_http_client)
            .await
            .map_err(|e| AuthError::OAuthError(e.to_string()))?;

        let access_token = token_result.access_token().secret();

        // Fetch user info from Google
        let user_info = providers::fetch_google_user(access_token).await?;

        // Store or update user in database if available
        if let Some(ref db) = self.database {
            let user_service = crate::services::UserService::new(db.clone());
            let db_user = user_service.get_or_create_oauth_user(
                user_info.email.clone(),
                user_info.email.split('@').next().unwrap_or("user").to_string(),
                user_info.name.clone(),
                user_info.picture.clone(),
                "google".to_string(),
                user_info.sub.clone(),
            ).await.map_err(|e| AuthError::AuthFailed(e.to_string()))?;

            // Update last login (already handled in user service)

            // Create a session
            let (_session, token) = db.sessions.create(
                db_user.id,
                None, // IP address would come from request context
                None, // User agent would come from request context
                24,   // 24 hours
            ).await.map_err(|e| AuthError::AuthFailed(e.to_string()))?;

            info!("User authenticated via Google: {}", user_info.email);
            return Ok(token);
        }

        // Fallback to in-memory session (backward compatibility)
        let session_id = Uuid::new_v4().to_string();
        let user = types::User {
            id: user_info.sub.clone(),
            email: user_info.email.clone(),
            provider: "google".to_string(),
        };

        let auth_session = types::AuthSession {
            session_id: session_id.clone(),
            user,
            created_at: chrono::Utc::now().timestamp(),
            access_token: access_token.to_string(),
        };

        let mut sessions = self.sessions.write().await;
        sessions.insert(session_id.clone(), auth_session);

        info!("User authenticated via Google: {}", user_info.email);
        Ok(session_id)
    }

    /// Get session by ID (supports both database and in-memory)
    pub async fn get_session(&self, session_id: &str) -> Result<Option<types::User>, AuthError> {
        // Check database first if available
        if let Some(ref db) = self.database {
            if let Ok(Some(session)) = db.sessions.validate(session_id).await {
                if let Ok(Some(user)) = db.users.get_by_id(session.user_id).await {
                    return Ok(Some(types::User {
                        id: user.id.to_string(),
                        email: user.email,
                        provider: user.auth_provider,
                    }));
                }
            }
        }

        // Fallback to in-memory sessions
        let sessions = self.sessions.read().await;
        Ok(sessions.get(session_id).map(|s| s.user.clone()))
    }

    /// Clean up expired sessions
    pub async fn cleanup_sessions(&self) {
        // Clean up database sessions if available
        if let Some(ref db) = self.database {
            if let Ok(count) = db.sessions.cleanup_expired().await {
                debug!("Cleaned up {} expired database sessions", count);
            }
        }

        // Clean up in-memory sessions
        let now = chrono::Utc::now().timestamp();
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|_, session| {
            // Keep sessions that are less than 24 hours old
            now - session.created_at < 86400
        });
        let removed = before - sessions.len();
        if removed > 0 {
            debug!("Cleaned up {} expired in-memory sessions", removed);
        }
    }
}