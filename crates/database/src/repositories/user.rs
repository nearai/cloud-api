use crate::models::User;
use crate::pool::DbPool;
use anyhow::{Result, Context};
use uuid::Uuid;
use chrono::Utc;
use tracing::debug;

pub struct UserRepository {
    pool: DbPool,
}

impl UserRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Create a new user (typically from OAuth)
    pub async fn create_from_oauth(
        &self,
        email: String,
        username: String,
        display_name: Option<String>,
        avatar_url: Option<String>,
        auth_provider: String,
        provider_user_id: String,
    ) -> Result<User> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let id = Uuid::new_v4();
        let now = Utc::now();
        
        let row = client.query_one(
            r#"
            INSERT INTO users (
                id, email, username, display_name, avatar_url,
                created_at, updated_at, is_active,
                auth_provider, provider_user_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, true, $8, $9)
            ON CONFLICT (email) DO UPDATE SET
                username = EXCLUDED.username,
                display_name = EXCLUDED.display_name,
                avatar_url = EXCLUDED.avatar_url,
                updated_at = EXCLUDED.updated_at,
                auth_provider = EXCLUDED.auth_provider,
                provider_user_id = EXCLUDED.provider_user_id
            RETURNING *
            "#,
            &[
                &id,
                &email,
                &username,
                &display_name,
                &avatar_url,
                &now,
                &now,
                &auth_provider,
                &provider_user_id,
            ],
        ).await.context("Failed to create user")?;
        
        debug!("Created/updated user: {} ({})", email, id);
        self.row_to_user(row)
    }

    /// Get a user by ID
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<User>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_opt(
            "SELECT * FROM users WHERE id = $1 AND is_active = true",
            &[&id],
        ).await.context("Failed to query user")?;
        
        match row {
            Some(row) => Ok(Some(self.row_to_user(row)?)),
            None => Ok(None),
        }
    }

    /// Get a user by email
    pub async fn get_by_email(&self, email: &str) -> Result<Option<User>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_opt(
            "SELECT * FROM users WHERE email = $1 AND is_active = true",
            &[&email],
        ).await.context("Failed to query user by email")?;
        
        match row {
            Some(row) => Ok(Some(self.row_to_user(row)?)),
            None => Ok(None),
        }
    }

    /// Get a user by OAuth provider details
    pub async fn get_by_provider(&self, auth_provider: &str, provider_user_id: &str) -> Result<Option<User>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_opt(
            "SELECT * FROM users WHERE auth_provider = $1 AND provider_user_id = $2 AND is_active = true",
            &[&auth_provider, &provider_user_id],
        ).await.context("Failed to query user by provider")?;
        
        match row {
            Some(row) => Ok(Some(self.row_to_user(row)?)),
            None => Ok(None),
        }
    }

    /// Update user's last login time
    pub async fn update_last_login(&self, id: Uuid) -> Result<()> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        client.execute(
            "UPDATE users SET last_login_at = NOW() WHERE id = $1",
            &[&id],
        ).await.context("Failed to update last login")?;
        
        Ok(())
    }

    /// Update user profile
    pub async fn update_profile(
        &self, 
        id: Uuid, 
        display_name: Option<String>,
        avatar_url: Option<String>
    ) -> Result<User> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_one(
            r#"
            UPDATE users
            SET display_name = COALESCE($2, display_name),
                avatar_url = COALESCE($3, avatar_url),
                updated_at = NOW()
            WHERE id = $1 AND is_active = true
            RETURNING *
            "#,
            &[&id, &display_name, &avatar_url],
        ).await.context("Failed to update user profile")?;
        
        debug!("Updated profile for user: {}", id);
        self.row_to_user(row)
    }

    /// List all users (with pagination)
    pub async fn list(&self, limit: i64, offset: i64) -> Result<Vec<User>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let rows = client.query(
            "SELECT * FROM users WHERE is_active = true ORDER BY created_at DESC LIMIT $1 OFFSET $2",
            &[&limit, &offset],
        ).await.context("Failed to list users")?;
        
        rows.into_iter()
            .map(|row| self.row_to_user(row))
            .collect()
    }

    /// Search users by username or email
    pub async fn search(&self, query: &str, limit: i64) -> Result<Vec<User>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let pattern = format!("%{}%", query);
        let rows = client.query(
            "SELECT * FROM users WHERE is_active = true AND (username ILIKE $1 OR email ILIKE $1) LIMIT $2",
            &[&pattern, &limit],
        ).await.context("Failed to search users")?;
        
        rows.into_iter()
            .map(|row| self.row_to_user(row))
            .collect()
    }

    /// Deactivate a user (soft delete)
    pub async fn deactivate(&self, id: Uuid) -> Result<bool> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let rows_affected = client.execute(
            "UPDATE users SET is_active = false WHERE id = $1",
            &[&id],
        ).await.context("Failed to deactivate user")?;
        
        Ok(rows_affected > 0)
    }

    // Helper function to convert database row to User
    fn row_to_user(&self, row: tokio_postgres::Row) -> Result<User> {
        Ok(User {
            id: row.get("id"),
            email: row.get("email"),
            username: row.get("username"),
            display_name: row.get("display_name"),
            avatar_url: row.get("avatar_url"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            last_login_at: row.get("last_login_at"),
            is_active: row.get("is_active"),
            auth_provider: row.get("auth_provider"),
            provider_user_id: row.get("provider_user_id"),
        })
    }
}