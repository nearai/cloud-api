use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::organization::OrganizationRepository;

// Domain ID types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct UserId(pub Uuid);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SessionToken(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceId(pub Uuid);

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
        user_agent: Option<String>,
        expires_in_hours: i64,
    ) -> anyhow::Result<(Session, String)>;

    async fn validate(&self, session_token: SessionToken) -> anyhow::Result<Option<Session>>;

    async fn get_by_id(&self, session_id: SessionId) -> anyhow::Result<Option<Session>>;

    async fn list_by_user(&self, user_id: UserId) -> anyhow::Result<Vec<Session>>;

    async fn extend(&self, session_id: SessionId, additional_hours: i64) -> anyhow::Result<bool>;

    async fn revoke(&self, session_id: SessionId) -> anyhow::Result<bool>;

    async fn revoke_all_for_user(&self, user_id: UserId) -> anyhow::Result<usize>;

    async fn cleanup_expired(&self) -> anyhow::Result<usize>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: ApiKeyId,
    // Returned only on creation
    pub key: Option<String>,
    /// First 8-10 characters of the key for display purposes (e.g., "sk_abc123")
    pub key_prefix: String,
    pub name: String,
    pub workspace_id: WorkspaceId,
    pub created_by_user_id: UserId,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    /// Optional spending limit in nano-dollars (scale 9, USD). None means no limit.
    pub spend_limit: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CreateApiKeyRequest {
    pub name: Option<String>,
    pub workspace_id: WorkspaceId,
    pub created_by_user_id: UserId,
    pub expires_at: Option<DateTime<Utc>>,
}

#[async_trait]
pub trait ApiKeyRepository: Send + Sync {
    async fn validate(&self, api_key: String) -> anyhow::Result<Option<ApiKey>>;

    async fn create(&self, request: CreateApiKeyRequest) -> anyhow::Result<ApiKey>;

    async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> anyhow::Result<Vec<ApiKey>>;

    async fn delete(&self, id: ApiKeyId) -> anyhow::Result<bool>;

    async fn update_last_used(&self, id: ApiKeyId) -> anyhow::Result<()>;

    async fn update_spend_limit(
        &self,
        id: ApiKeyId,
        spend_limit: Option<i64>,
    ) -> anyhow::Result<ApiKey>;
}

// Service interfaces
#[async_trait]
pub trait AuthServiceTrait: Send + Sync {
    /// Create a new session for a user
    async fn create_session(
        &self,
        user_id: UserId,
        ip_address: Option<String>,
        user_agent: Option<String>,
        expires_in_hours: i64,
    ) -> Result<(Session, String), AuthError>;

    /// Validate a session token and return the session
    async fn validate_session_token(
        &self,
        session_token: SessionToken,
    ) -> Result<Option<Session>, AuthError>;

    /// Validate a session token and return the associated user
    async fn validate_session(&self, session_token: SessionToken) -> Result<User, AuthError>;

    /// Get a user by their ID
    async fn get_user_by_id(&self, user_id: UserId) -> Result<User, AuthError>;

    /// Logout (revoke session)
    async fn logout(&self, session_id: SessionId) -> Result<bool, AuthError>;

    /// Get or create user from OAuth data
    async fn get_or_create_oauth_user(&self, oauth_info: OAuthUserInfo) -> Result<User, AuthError>;

    /// Clean up expired sessions
    async fn cleanup_expired_sessions(&self) -> Result<usize, AuthError>;

    /// Validate an API key and return the associated user
    async fn validate_api_key(&self, api_key: String) -> Result<ApiKey, AuthError>;

    /// Create an API key for a workspace with proper permission checking
    async fn create_workspace_api_key(
        &self,
        request: CreateApiKeyRequest,
    ) -> Result<ApiKey, AuthError>;

    /// Check if a user can manage API keys for a workspace
    async fn can_manage_workspace_api_keys(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<bool, AuthError>;

    /// List API keys for a workspace with proper permission checking
    async fn list_workspace_api_keys(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
    ) -> Result<Vec<ApiKey>, AuthError>;
}

pub struct AuthService {
    pub user_repository: Arc<dyn UserRepository>,
    pub session_repository: Arc<dyn SessionRepository>,
    pub api_key_repository: Arc<dyn ApiKeyRepository>,
    pub organization_repository: Arc<dyn OrganizationRepository>,
    pub workspace_repository: Arc<dyn WorkspaceRepository>,
}

/// Workspace domain model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub display_name: String,
    pub description: Option<String>,
    pub organization_id: crate::organization::OrganizationId,
    pub created_by_user_id: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub is_active: bool,
    pub settings: Option<serde_json::Value>,
}

/// Workspace repository trait for auth service
#[async_trait]
pub trait WorkspaceRepository: Send + Sync {
    async fn get_workspace_with_organization(
        &self,
        workspace_id: WorkspaceId,
    ) -> anyhow::Result<Option<(Workspace, crate::organization::Organization)>>;
    async fn get_by_id(&self, workspace_id: WorkspaceId) -> anyhow::Result<Option<Workspace>>;
}

pub struct UserService {
    pub user_repository: Arc<dyn UserRepository>,
}

// Mock constants for testing
const MOCK_USER_ID: &str = "11111111-1111-1111-1111-111111111111";

/// Mock auth service that returns fake data for testing/development
/// Used when mock auth is enabled
pub struct MockAuthService {
    pub apikey_repository: Arc<dyn ApiKeyRepository>,
}

impl MockAuthService {
    fn create_mock_user() -> User {
        User {
            id: UserId(uuid::Uuid::parse_str(MOCK_USER_ID).expect("Invalid mock user ID")),
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
        }
    }

    fn create_mock_session(&self, user_id: UserId) -> (Session, String) {
        let session_id = SessionId(uuid::Uuid::new_v4());
        let session_token = uuid::Uuid::new_v4();
        let expires_at = chrono::Utc::now() + chrono::Duration::hours(24);

        let session = Session {
            id: session_id,
            user_id,
            token_hash: "mock_token_hash".to_string(),
            created_at: chrono::Utc::now(),
            expires_at,
            ip_address: Some("127.0.0.1".to_string()),
            user_agent: Some("Mock User Agent".to_string()),
        };

        (session, session_token.to_string())
    }
}

#[async_trait]
impl AuthServiceTrait for MockAuthService {
    async fn create_session(
        &self,
        user_id: UserId,
        _ip_address: Option<String>,
        _user_agent: Option<String>,
        _expires_in_hours: i64,
    ) -> Result<(Session, String), AuthError> {
        Ok(self.create_mock_session(user_id))
    }

    async fn validate_session_token(
        &self,
        _session_token: SessionToken,
    ) -> Result<Option<Session>, AuthError> {
        let mock_user = Self::create_mock_user();
        let (session, _) = self.create_mock_session(mock_user.id);
        Ok(Some(session))
    }

    async fn validate_session(&self, session_token: SessionToken) -> Result<User, AuthError> {
        tracing::debug!(
            "MockAuthService::validate_session called with token: {}",
            session_token
        );
        let user = Self::create_mock_user();
        tracing::debug!("MockAuthService returning mock user: {}", user.email);
        Ok(user)
    }

    async fn get_user_by_id(&self, _user_id: UserId) -> Result<User, AuthError> {
        Ok(Self::create_mock_user())
    }

    async fn logout(&self, _session_id: SessionId) -> Result<bool, AuthError> {
        Ok(true) // Mock logout always succeeds
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

    async fn create_workspace_api_key(
        &self,
        request: CreateApiKeyRequest,
    ) -> Result<ApiKey, AuthError> {
        Ok(self.apikey_repository.create(request).await.unwrap())
    }

    async fn can_manage_workspace_api_keys(
        &self,
        _workspace_id: WorkspaceId,
        _user_id: UserId,
    ) -> Result<bool, AuthError> {
        Ok(true) // Mock user can always manage API keys
    }

    async fn list_workspace_api_keys(
        &self,
        workspace_id: WorkspaceId,
        _requester_id: UserId,
    ) -> Result<Vec<ApiKey>, AuthError> {
        Ok(self
            .apikey_repository
            .list_by_workspace(workspace_id)
            .await
            .unwrap())
    }
}
