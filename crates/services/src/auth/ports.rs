use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::organization::{OrganizationId, OrganizationRepository};

// Domain ID types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct UserId(pub Uuid);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyId(pub String);

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

// Domain models
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub email: String,
    pub username: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub organization_id: Option<OrganizationId>,
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

    async fn validate(&self, session_token: Uuid) -> anyhow::Result<Option<Session>>;

    async fn get_by_id(&self, id: UserId) -> anyhow::Result<Option<Session>>;

    async fn list_by_user(&self, user_id: UserId) -> anyhow::Result<Vec<Session>>;

    async fn extend(&self, session_id: SessionId, additional_hours: i64) -> anyhow::Result<bool>;

    async fn revoke(&self, session_id: SessionId) -> anyhow::Result<bool>;

    async fn revoke_all_for_user(&self, user_id: UserId) -> anyhow::Result<usize>;

    async fn cleanup_expired(&self) -> anyhow::Result<usize>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: ApiKeyId,
    pub name: String,
    pub organization_id: OrganizationId,
    pub created_by_user_id: UserId,
    pub account_type: AccountType,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub is_active: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: Option<String>,
    pub organization_id: OrganizationId,
    pub account_type: AccountType,
    pub created_by_user_id: UserId,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AccountType {
    User,
    ServiceAccount,
}

impl std::fmt::Display for AccountType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                AccountType::User => "User",
                AccountType::ServiceAccount => "ServiceAccount",
            }
        )
    }
}

impl From<String> for AccountType {
    fn from(s: String) -> Self {
        match s.as_str() {
            "User" => AccountType::User,
            "ServiceAccount" => AccountType::ServiceAccount,
            _ => panic!("Invalid account_type: {}", s),
        }
    }
}

#[async_trait]
pub trait ApiKeyRepository: Send + Sync {
    async fn validate(&self, api_key: String) -> anyhow::Result<Option<ApiKey>>;

    async fn create(&self, request: CreateApiKeyRequest) -> anyhow::Result<ApiKey>;

    async fn list_by_organization(
        &self,
        organization_id: OrganizationId,
    ) -> anyhow::Result<Vec<ApiKey>>;

    async fn delete(&self, id: ApiKeyId) -> anyhow::Result<bool>;
    async fn update_last_used(&self, id: ApiKeyId) -> anyhow::Result<()>;
}

// Service interfaces
pub struct AuthService {
    pub user_repository: Arc<dyn UserRepository>,
    pub session_repository: Arc<dyn SessionRepository>,
    pub api_key_repository: Arc<dyn ApiKeyRepository>,
    pub organization_repository: Arc<dyn OrganizationRepository>,
}

pub struct UserService {
    pub user_repository: Arc<dyn UserRepository>,
}
