use database::Database;
use anyhow::Result;
use tracing::{debug, info};
use std::sync::Arc;

/// Service for handling user operations
pub struct UserService {
    db: Arc<Database>,
}

impl UserService {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
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
    ) -> Result<database::User> {
        // Check if user already exists
        let existing_user = self.db.users.get_by_email(&email).await?;
        
        if let Some(_user) = existing_user {
            // User exists, just update and return
            debug!("Updating existing user: {}", email);
            let updated_user = self.db.users.create_from_oauth(
                email,
                username,
                display_name,
                avatar_url,
                auth_provider,
                provider_user_id,
            ).await?;
            
            // Update last login
            self.db.users.update_last_login(updated_user.id).await.ok();
            return Ok(updated_user);
        }
        
        // New user - create user
        debug!("Creating new user: {}", email);
        
        let user = self.db.users.create_from_oauth(
            email.clone(),
            username,
            display_name,
            avatar_url,
            auth_provider,
            provider_user_id,
        ).await?;
        
        info!("Created new user {}", email);
        Ok(user)
    }
}