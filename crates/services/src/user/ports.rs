use crate::auth::ports::{Session, SessionId, User, UserId};
use crate::organization::Organization;
use crate::workspace::{ApiKey, Workspace};
use async_trait::async_trait;

/// Errors that can occur during user service operations
#[derive(Debug, thiserror::Error)]
pub enum UserServiceError {
    #[error("User not found")]
    UserNotFound,

    #[error("Session not found")]
    SessionNotFound,

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Invalid parameters: {0}")]
    InvalidParams(String),

    #[error("Internal error: {0}")]
    InternalError(String),

    #[error("Organization already exists")]
    OrganizationAlreadyExists,
}

/// Response from quick setup containing all created resources
#[derive(Debug, Clone)]
pub struct QuickSetupResult {
    pub organization: Organization,
    pub workspace: Workspace,
    pub api_key: ApiKey,
}

/// Service trait for user profile and session management
#[async_trait]
pub trait UserServiceTrait: Send + Sync {
    /// Get a user by their ID
    async fn get_user(&self, user_id: UserId) -> Result<User, UserServiceError>;

    /// Update a user's profile
    async fn update_profile(
        &self,
        user_id: UserId,
        display_name: Option<String>,
        avatar_url: Option<String>,
    ) -> Result<User, UserServiceError>;

    /// Get all sessions for a user
    async fn get_user_sessions(&self, user_id: UserId) -> Result<Vec<Session>, UserServiceError>;

    /// Revoke a specific session (with authorization check)
    async fn revoke_session(
        &self,
        user_id: UserId,
        session_id: SessionId,
    ) -> Result<bool, UserServiceError>;

    /// Revoke all sessions for a user
    async fn revoke_all_sessions(&self, user_id: UserId) -> Result<usize, UserServiceError>;

    /// Quick setup: Create organization, workspace, and API key for a user
    ///
    /// This is a convenience method that creates all resources needed for a user to get started.
    /// The organization name is derived from the user's email (e.g., "user@example.com" -> "user-org").
    async fn quick_setup(&self, user_id: UserId) -> Result<QuickSetupResult, UserServiceError>;
}
