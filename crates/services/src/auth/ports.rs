use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::organization::OrganizationRepository;
use crate::workspace::{ApiKey, ApiKeyRepository, WorkspaceId, WorkspaceRepository};

// Domain ID types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct UserId(pub Uuid);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SessionToken(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessTokenClaims {
    pub sub: UserId,
    pub exp: i64,
    pub iat: i64,
}

impl From<Uuid> for UserId {
    fn from(uuid: Uuid) -> Self {
        UserId(uuid)
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<Uuid> for SessionId {
    fn from(uuid: Uuid) -> Self {
        SessionId(uuid)
    }
}

impl std::fmt::Display for SessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// Domain models
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub email: String,
    pub username: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub auth_provider: String,
    pub role: UserRole,
    pub is_active: bool,
    pub last_login: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub tokens_revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    User,
    Admin,
    SuperAdmin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSession {
    pub session_id: SessionId,
    pub user_id: UserId,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthUserInfo {
    pub provider: String,
    pub provider_user_id: String,
    pub email: String,
    pub username: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

// Error types
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("OAuth error: {0}")]
    OAuthError(String),

    #[error("Invalid state parameter")]
    InvalidState,

    #[error("Authentication failed: {0}")]
    AuthFailed(String),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Network error: {0}")]
    NetworkError(String),

    #[error("Session not found")]
    SessionNotFound,

    #[error("User not found")]
    UserNotFound,

    #[error("Internal error: {0}")]
    InternalError(String),

    #[error("Unauthorized")]
    Unauthorized,

    #[error("Invalid user agent")]
    InvalidUserAgent,

    #[error("User agent is too long (max {0} chars)")]
    UserAgentTooLong(usize),
}

// Repository traits
#[async_trait]
pub trait UserRepository: Send + Sync {
    async fn create(
        &self,
        email: String,
        username: String,
        display_name: Option<String>,
        avatar_url: Option<String>,
    ) -> anyhow::Result<User>;

    async fn create_from_oauth(
        &self,
        email: String,
        username: String,
        display_name: Option<String>,
        avatar_url: Option<String>,
        auth_provider: String,
        provider_user_id: String,
    ) -> anyhow::Result<User>;

    async fn get_by_id(&self, id: UserId) -> anyhow::Result<Option<User>>;

    async fn get_by_email(&self, email: &str) -> anyhow::Result<Option<User>>;

    async fn update(
        &self,
        id: UserId,
        display_name: Option<String>,
        avatar_url: Option<String>,
    ) -> anyhow::Result<Option<User>>;

    async fn update_last_login(&self, id: UserId) -> anyhow::Result<()>;

    async fn update_tokens_revoked_at(&self, id: UserId) -> anyhow::Result<()>;

    async fn delete(&self, id: UserId) -> anyhow::Result<bool>;

    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<User>>;
}

/// Session for OAuth authentication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub user_id: UserId,
    pub token_hash: String, // Store hashed session token
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
}

#[async_trait]
pub trait SessionRepository: Send + Sync {
    async fn create(
        &self,
        user_id: UserId,
        ip_address: Option<String>,
        user_agent: String,
        expires_in_hours: i64,
    ) -> anyhow::Result<(Session, String)>;

    async fn validate(
        &self,
        session_token: SessionToken,
        user_agent: &str,
    ) -> anyhow::Result<Option<Session>>;

    async fn get_by_id(&self, session_id: SessionId) -> anyhow::Result<Option<Session>>;

    async fn list_by_user(&self, user_id: UserId) -> anyhow::Result<Vec<Session>>;

    async fn extend(&self, session_id: SessionId, additional_hours: i64) -> anyhow::Result<bool>;

    async fn rotate(
        &self,
        session_id: SessionId,
        expires_in_hours: i64,
    ) -> anyhow::Result<(Session, String)>;

    async fn revoke(&self, session_id: SessionId) -> anyhow::Result<bool>;

    async fn revoke_all_for_user(&self, user_id: UserId) -> anyhow::Result<usize>;

    async fn cleanup_expired(&self) -> anyhow::Result<usize>;
}

// Service interfaces
#[async_trait]
pub trait AuthServiceTrait: Send + Sync {
    /// Create a new session for a user
    async fn create_session(
        &self,
        user_id: UserId,
        ip_address: Option<String>,
        user_agent: String,
        encoding_key: String,
        expires_in_hours: i64,
        refresh_expires_in_hours: i64,
    ) -> Result<(String, Session, String), AuthError>;

    fn create_session_access_token(
        &self,
        user_id: UserId,
        encoding_key: String,
        expires_in_hours: i64,
    ) -> Result<String, AuthError>;

    fn validate_session_access_token(
        &self,
        access_token: String,
        encoding_key: String,
    ) -> Result<Option<AccessTokenClaims>, AuthError>;

    async fn validate_session_access(
        &self,
        access_token: String,
        encoding_key: String,
    ) -> Result<User, AuthError>;

    /// Validate a session token and return the session
    async fn validate_session_refresh_token(
        &self,
        session_token: SessionToken,
        user_agent: &str,
    ) -> Result<Option<Session>, AuthError>;

    /// Validate a session token and return the associated user
    async fn validate_session_refresh(
        &self,
        session_token: SessionToken,
        user_agent: &str,
    ) -> Result<(Session, User), AuthError>;

    /// Get a user by their ID
    async fn get_user_by_id(&self, user_id: UserId) -> Result<User, AuthError>;

    /// Logout (revoke session)
    async fn logout(&self, session_id: SessionId) -> Result<bool, AuthError>;

    /// Rotate a refresh token session (refresh token rotation)
    /// This atomically updates the token hash and expiration, ensuring only one valid token at a time
    async fn rotate_session(
        &self,
        user_id: UserId,
        session_id: SessionId,
        encoding_key: String,
        access_token_expires_in_hours: i64,
        refresh_token_expires_in_hours: i64,
    ) -> Result<(String, Session, String), AuthError>;

    /// Get or create user from OAuth data
    async fn get_or_create_oauth_user(&self, oauth_info: OAuthUserInfo) -> Result<User, AuthError>;

    /// Clean up expired sessions
    async fn cleanup_expired_sessions(&self) -> Result<usize, AuthError>;

    /// Validate an API key and return the associated user
    async fn validate_api_key(&self, api_key: String) -> Result<ApiKey, AuthError>;

    /// Check if a user can manage API keys for a workspace
    async fn can_manage_workspace_api_keys(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<bool, AuthError>;
}

pub struct AuthService {
    pub user_repository: Arc<dyn UserRepository>,
    pub session_repository: Arc<dyn SessionRepository>,
    pub api_key_repository: Arc<dyn ApiKeyRepository>,
    pub organization_repository: Arc<dyn OrganizationRepository>,
    pub workspace_repository: Arc<dyn WorkspaceRepository>,
    pub organization_service: Arc<dyn crate::organization::OrganizationServiceTrait>,
}

pub struct UserService {
    pub user_repository: Arc<dyn UserRepository>,
}

// Mock constants for testing
const MOCK_USER_ID: &str = "11111111-1111-1111-1111-111111111111";
pub const MOCK_USER_AGENT: &str = "Mock User Agent";

/// Mock auth service that returns fake data for testing/development
/// Used when mock auth is enabled
pub struct MockAuthService {
    pub apikey_repository: Arc<dyn ApiKeyRepository>,
}

impl MockAuthService {
    fn create_mock_user() -> User {
        let id = UserId(
            uuid::Uuid::parse_str(crate::auth::ports::MOCK_USER_ID).expect("Invalid mock user ID"),
        );
        Self::create_mock_user_with_id(id)
    }

    fn create_mock_user_with_id(id: UserId) -> User {
        User {
            id,
            email: "admin@test.com".to_string(),
            username: "testuser".to_string(),
            display_name: Some("Test User".to_string()),
            avatar_url: Some("https://example.com/avatar.jpg".to_string()),
            auth_provider: "mock".to_string(),
            role: UserRole::User,
            is_active: true,
            last_login: Some(chrono::Utc::now()),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            tokens_revoked_at: None,
        }
    }

    fn create_mock_session(&self, user_id: UserId) -> (String, Session, String) {
        self.create_mock_session_with_params(
            user_id,
            None,
            Some(MOCK_USER_AGENT.to_string()),
            "mock_encoding_key".to_string(),
            1,
            7 * 24,
        )
    }

    fn create_mock_session_with_params(
        &self,
        user_id: UserId,
        ip_address: Option<String>,
        user_agent: Option<String>,
        encoding_key: String,
        expires_in_hours: i64,
        refresh_expires_in_hours: i64,
    ) -> (String, Session, String) {
        let expiration = chrono::Utc::now() + chrono::Duration::hours(expires_in_hours);

        let claims = AccessTokenClaims {
            sub: user_id.clone(),
            exp: expiration.timestamp(),
            iat: chrono::Utc::now().timestamp(),
        };

        let access_token = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(encoding_key.as_bytes()),
        )
        .unwrap();

        let session_id = SessionId(uuid::Uuid::new_v4());
        // Generate token with same format as real session repository
        let session_token = format!("rt_{}", uuid::Uuid::new_v4().to_string().replace("-", ""));
        let expires_at = chrono::Utc::now() + chrono::Duration::hours(refresh_expires_in_hours);

        let session = Session {
            id: session_id,
            user_id,
            token_hash: "mock_token_hash".to_string(),
            created_at: chrono::Utc::now(),
            expires_at,
            ip_address: ip_address.or(Some("127.0.0.1".to_string())),
            user_agent: user_agent.or(Some(MOCK_USER_AGENT.to_string())),
        };

        (access_token, session, session_token)
    }
}

#[async_trait]
impl AuthServiceTrait for MockAuthService {
    async fn create_session(
        &self,
        user_id: UserId,
        ip_address: Option<String>,
        user_agent: String,
        encoding_key: String,
        expires_in_hours: i64,
        refresh_expires_in_hours: i64,
    ) -> Result<(String, Session, String), AuthError> {
        Ok(self.create_mock_session_with_params(
            user_id,
            ip_address,
            Some(user_agent),
            encoding_key,
            expires_in_hours,
            refresh_expires_in_hours,
        ))
    }

    fn create_session_access_token(
        &self,
        user_id: UserId,
        encoding_key: String,
        expires_in_hours: i64,
    ) -> Result<String, AuthError> {
        let expiration = chrono::Utc::now() + chrono::Duration::hours(expires_in_hours);

        let claims = AccessTokenClaims {
            sub: user_id,
            exp: expiration.timestamp(),
            iat: chrono::Utc::now().timestamp(),
        };

        jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(encoding_key.as_bytes()),
        )
        .map_err(|e| AuthError::InternalError(format!("Failed to create jwt: {e}")))
    }

    fn validate_session_access_token(
        &self,
        access_token: String,
        encoding_key: String,
    ) -> Result<Option<AccessTokenClaims>, AuthError> {
        // Allow any string, no exp checking (useful for testing)
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.validate_exp = false;

        let claims = if let Ok(claims) = jsonwebtoken::decode::<AccessTokenClaims>(
            access_token,
            &jsonwebtoken::DecodingKey::from_secret(encoding_key.as_bytes()),
            &validation,
        ) {
            claims
        } else {
            return Ok(None);
        };

        Ok(Some(claims.claims))
    }

    async fn validate_session_access(
        &self,
        access_token: String,
        encoding_key: String,
    ) -> Result<User, AuthError> {
        match self.validate_session_access_token(access_token, encoding_key) {
            Ok(Some(claims)) => {
                let user = Self::create_mock_user_with_id(claims.sub);
                tracing::debug!("MockAuthService returning mock user: {}", user.email);
                Ok(user)
            }
            Ok(None) => {
                let user = Self::create_mock_user();
                tracing::debug!("MockAuthService returning mock user: {}", user.email);
                Ok(user)
            }
            Err(_) => Err(AuthError::SessionNotFound),
        }
    }

    async fn validate_session_refresh_token(
        &self,
        session_token: SessionToken,
        user_agent: &str,
    ) -> Result<Option<Session>, AuthError> {
        // Accept the known test session token or any token that starts with "rt_"
        if session_token.0.starts_with("rt_") && user_agent == MOCK_USER_AGENT {
            let mock_user = Self::create_mock_user();
            let (_access_token, refresh_session, _refresh_token) =
                self.create_mock_session(mock_user.id);
            Ok(Some(refresh_session))
        } else {
            Ok(None)
        }
    }

    async fn validate_session_refresh(
        &self,
        session_token: SessionToken,
        user_agent: &str,
    ) -> Result<(Session, User), AuthError> {
        tracing::debug!(
            "MockAuthService::validate_session called with token: {}, user_agent: {}",
            session_token,
            user_agent
        );
        // Accept the known test session token or any token that starts with "rt_"
        if session_token.0.starts_with("rt_") && user_agent == MOCK_USER_AGENT {
            let user = Self::create_mock_user();
            let (_, session, _) = self.create_mock_session(user.id.clone());
            tracing::debug!("MockAuthService returning mock user: {}", user.email);
            Ok((session, user))
        } else {
            Err(AuthError::SessionNotFound)
        }
    }

    async fn get_user_by_id(&self, _user_id: UserId) -> Result<User, AuthError> {
        Ok(Self::create_mock_user())
    }

    async fn logout(&self, _session_id: SessionId) -> Result<bool, AuthError> {
        Ok(true) // Mock logout always succeeds
    }

    async fn rotate_session(
        &self,
        _user_id: UserId,
        _session_id: SessionId,
        encoding_key: String,
        access_token_expires_in_hours: i64,
        refresh_token_expires_in_hours: i64,
    ) -> Result<(String, Session, String), AuthError> {
        // Create a mock session rotation
        let mock_user = Self::create_mock_user();
        let (access_token, refresh_session, refresh_token) = self.create_mock_session_with_params(
            mock_user.id,
            None,
            Some(MOCK_USER_AGENT.to_string()),
            encoding_key,
            access_token_expires_in_hours,
            refresh_token_expires_in_hours,
        );
        Ok((access_token, refresh_session, refresh_token))
    }

    async fn get_or_create_oauth_user(
        &self,
        _oauth_info: OAuthUserInfo,
    ) -> Result<User, AuthError> {
        Ok(Self::create_mock_user())
    }

    async fn cleanup_expired_sessions(&self) -> Result<usize, AuthError> {
        Ok(0) // No sessions to clean up in mock
    }

    async fn validate_api_key(&self, api_key: String) -> Result<ApiKey, AuthError> {
        self.apikey_repository
            .validate(api_key)
            .await
            .unwrap()
            .ok_or(AuthError::Unauthorized)
    }

    async fn can_manage_workspace_api_keys(
        &self,
        _workspace_id: WorkspaceId,
        _user_id: UserId,
    ) -> Result<bool, AuthError> {
        Ok(true) // Mock user can always manage API keys
    }
}
