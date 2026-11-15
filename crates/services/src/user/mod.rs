use crate::auth::ports::{Session, SessionId, SessionRepository, User, UserId, UserRepository};
use async_trait::async_trait;
use std::sync::Arc;
use tracing::debug;

pub mod ports;
pub use ports::*;

/// UserService handles user profile and session management operations
pub struct UserService {
    user_repository: Arc<dyn UserRepository>,
    session_repository: Arc<dyn SessionRepository>,
}

impl UserService {
    pub fn new(
        user_repository: Arc<dyn UserRepository>,
        session_repository: Arc<dyn SessionRepository>,
    ) -> Self {
        Self {
            user_repository,
            session_repository,
        }
    }
}

#[async_trait]
impl UserServiceTrait for UserService {
    async fn get_user(&self, user_id: UserId) -> Result<User, UserServiceError> {
        debug!("Getting user: {}", user_id);

        self.user_repository
            .get_by_id(user_id)
            .await
            .map_err(|e| UserServiceError::InternalError(format!("Failed to get user: {e}")))?
            .ok_or(UserServiceError::UserNotFound)
    }

    async fn get_user_sessions(&self, user_id: UserId) -> Result<Vec<Session>, UserServiceError> {
        debug!("Getting sessions for user: {}", user_id);

        self.session_repository
            .list_by_user(user_id)
            .await
            .map_err(|e| {
                UserServiceError::InternalError(format!("Failed to get user sessions: {e}"))
            })
    }

    async fn revoke_session(
        &self,
        user_id: UserId,
        session_id: SessionId,
    ) -> Result<bool, UserServiceError> {
        debug!("Revoking session: {} for user: {}", session_id, user_id);

        // Verify the session belongs to the user
        let session = self
            .session_repository
            .get_by_id(session_id.clone())
            .await
            .map_err(|e| UserServiceError::InternalError(format!("Failed to get session: {e}")))?
            .ok_or(UserServiceError::SessionNotFound)?;

        if session.user_id != user_id {
            return Err(UserServiceError::Unauthorized(
                "Session does not belong to user".to_string(),
            ));
        }

        self.session_repository
            .revoke(session_id)
            .await
            .map_err(|e| UserServiceError::InternalError(format!("Failed to revoke session: {e}")))
    }
}
