use crate::{
    auth::ports::{Session, SessionId, User, UserId},
    organization::Organization,
};
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

    /// Get all organizations that a user is a member of
    async fn get_user_organizations(
        &self,
        user_id: UserId,
    ) -> Result<Vec<Organization>, UserServiceError>;

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
}
