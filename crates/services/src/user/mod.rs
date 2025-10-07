use crate::{
    auth::ports::{Session, SessionId, SessionRepository, User, UserId, UserRepository},
    organization::{Organization, OrganizationRepository},
};
use async_trait::async_trait;
use std::sync::Arc;
use tracing::debug;

pub mod ports;
pub use ports::*;

/// UserService handles user profile and session management operations
pub struct UserService {
    user_repository: Arc<dyn UserRepository>,
    session_repository: Arc<dyn SessionRepository>,
    organization_repository: Arc<dyn OrganizationRepository>,
}

impl UserService {
    pub fn new(
        user_repository: Arc<dyn UserRepository>,
        session_repository: Arc<dyn SessionRepository>,
        organization_repository: Arc<dyn OrganizationRepository>,
    ) -> Self {
        Self {
            user_repository,
            session_repository,
            organization_repository,
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
            .map_err(|e| UserServiceError::InternalError(format!("Failed to get user: {}", e)))?
            .ok_or(UserServiceError::UserNotFound)
    }

    async fn update_profile(
        &self,
        user_id: UserId,
        display_name: Option<String>,
        avatar_url: Option<String>,
    ) -> Result<User, UserServiceError> {
        debug!("Updating profile for user: {}", user_id);

        self.user_repository
            .update(user_id, display_name, avatar_url)
            .await
            .map_err(|e| {
                UserServiceError::InternalError(format!("Failed to update profile: {}", e))
            })?
            .ok_or(UserServiceError::UserNotFound)
    }

    async fn get_user_organizations(
        &self,
        user_id: UserId,
    ) -> Result<Vec<Organization>, UserServiceError> {
        debug!("Getting organizations for user: {}", user_id);

        // Get all active organizations for the user
        // Using a large limit since most users won't have hundreds of orgs
        self.organization_repository
            .list_organizations_by_user(user_id.0, 1000, 0)
            .await
            .map_err(|e| {
                UserServiceError::InternalError(format!("Failed to get user organizations: {}", e))
            })
    }

    async fn get_user_sessions(&self, user_id: UserId) -> Result<Vec<Session>, UserServiceError> {
        debug!("Getting sessions for user: {}", user_id);

        self.session_repository
            .list_by_user(user_id)
            .await
            .map_err(|e| {
                UserServiceError::InternalError(format!("Failed to get user sessions: {}", e))
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
            .map_err(|e| UserServiceError::InternalError(format!("Failed to get session: {}", e)))?
            .ok_or(UserServiceError::SessionNotFound)?;

        if session.user_id != user_id {
            return Err(UserServiceError::Unauthorized(
                "Session does not belong to user".to_string(),
            ));
        }

        self.session_repository
            .revoke(session_id)
            .await
            .map_err(|e| {
                UserServiceError::InternalError(format!("Failed to revoke session: {}", e))
            })
    }

    async fn revoke_all_sessions(&self, user_id: UserId) -> Result<usize, UserServiceError> {
        debug!("Revoking all sessions for user: {}", user_id);

        self.session_repository
            .revoke_all_for_user(user_id)
            .await
            .map_err(|e| {
                UserServiceError::InternalError(format!(
                    "Failed to revoke all user sessions: {}",
                    e
                ))
            })
    }
}
