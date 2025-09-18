use super::ports::{AuthError, User, UserId, UserRepository};
use std::sync::Arc;
use tracing::{debug, info};

pub struct UserServiceImpl {
    user_repository: Arc<dyn UserRepository>,
}

impl UserServiceImpl {
    pub fn new(user_repository: Arc<dyn UserRepository>) -> Self {
        Self { user_repository }
    }

    /// Get or create user from OAuth data
    pub async fn get_or_create_oauth_user(
        &self,
        email: String,
        username: String,
        display_name: Option<String>,
        avatar_url: Option<String>,
        auth_provider: String,
        provider_user_id: String,
    ) -> Result<User, AuthError> {
        // Check if user already exists
        let existing_user = self
            .user_repository
            .get_by_email(&email)
            .await
            .map_err(|e| {
                AuthError::InternalError(format!("Failed to check existing user: {}", e))
            })?;

        if let Some(user) = existing_user {
            // User exists, update and return
            debug!("Updating existing user: {}", email);

            // Update last login
            self.user_repository
                .update_last_login(user.id.clone())
                .await
                .ok();

            // Update user info if changed
            if user.display_name != display_name || user.avatar_url != avatar_url {
                let updated_user = self
                    .user_repository
                    .update(user.id.clone(), display_name, avatar_url)
                    .await
                    .map_err(|e| {
                        AuthError::InternalError(format!("Failed to update user: {}", e))
                    })?;

                if let Some(updated) = updated_user {
                    return Ok(updated);
                }
            }

            return Ok(user);
        }

        // New user - create user
        debug!("Creating new user: {}", email);

        let user = self
            .user_repository
            .create_from_oauth(
                email.clone(),
                username,
                display_name,
                avatar_url,
                auth_provider,
                provider_user_id,
            )
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to create user: {}", e)))?;

        info!("Created new user {}", email);
        Ok(user)
    }

    /// Get a user by ID
    pub async fn get_user(&self, user_id: UserId) -> Result<Option<User>, AuthError> {
        self.user_repository
            .get_by_id(user_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to get user: {}", e)))
    }

    /// Get a user by email
    pub async fn get_user_by_email(&self, email: &str) -> Result<Option<User>, AuthError> {
        self.user_repository
            .get_by_email(email)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to get user by email: {}", e)))
    }

    /// Update a user's profile
    pub async fn update_user(
        &self,
        user_id: UserId,
        display_name: Option<String>,
        avatar_url: Option<String>,
    ) -> Result<Option<User>, AuthError> {
        self.user_repository
            .update(user_id, display_name, avatar_url)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to update user: {}", e)))
    }

    /// Delete a user
    pub async fn delete_user(&self, user_id: UserId) -> Result<bool, AuthError> {
        self.user_repository
            .delete(user_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to delete user: {}", e)))
    }

    /// List users with pagination
    pub async fn list_users(&self, limit: i64, offset: i64) -> Result<Vec<User>, AuthError> {
        self.user_repository
            .list(limit, offset)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to list users: {}", e)))
    }
}
