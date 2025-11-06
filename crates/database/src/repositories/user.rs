use crate::models::User;
use crate::pool::DbPool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use tracing::debug;
use uuid::Uuid;

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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let id = Uuid::new_v4();
        let now = Utc::now();

        let row = client
            .query_one(
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
            )
            .await
            .context("Failed to create user")?;

        debug!("Created/updated user: {} ({})", email, id);
        self.row_to_user(row)
    }

    /// Get a user by ID
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<User>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                "SELECT * FROM users WHERE id = $1 AND is_active = true",
                &[&id],
            )
            .await
            .context("Failed to query user")?;

        match row {
            Some(row) => Ok(Some(self.row_to_user(row)?)),
            None => Ok(None),
        }
    }

    /// Get a user by email
    pub async fn get_by_email(&self, email: &str) -> Result<Option<User>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                "SELECT * FROM users WHERE email = $1 AND is_active = true",
                &[&email],
            )
            .await
            .context("Failed to query user by email")?;

        match row {
            Some(row) => Ok(Some(self.row_to_user(row)?)),
            None => Ok(None),
        }
    }

    /// Get a user by OAuth provider details
    pub async fn get_by_provider(
        &self,
        auth_provider: &str,
        provider_user_id: &str,
    ) -> Result<Option<User>> {
        let client = self
            .pool
            .get()
            .await
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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        client
            .execute(
                "UPDATE users SET last_login_at = NOW() WHERE id = $1",
                &[&id],
            )
            .await
            .context("Failed to update last login")?;

        Ok(())
    }

    /// Update user profile
    pub async fn update_profile(
        &self,
        id: Uuid,
        display_name: Option<String>,
        avatar_url: Option<String>,
    ) -> Result<User> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_one(
                r#"
            UPDATE users
            SET display_name = COALESCE($2, display_name),
                avatar_url = COALESCE($3, avatar_url),
                updated_at = NOW()
            WHERE id = $1 AND is_active = true
            RETURNING *
            "#,
                &[&id, &display_name, &avatar_url],
            )
            .await
            .context("Failed to update user profile")?;

        debug!("Updated profile for user: {}", id);
        self.row_to_user(row)
    }

    /// List all users (with pagination)
    pub async fn list(&self, limit: i64, offset: i64) -> Result<Vec<User>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client.query(
            "SELECT * FROM users WHERE is_active = true ORDER BY created_at DESC LIMIT $1 OFFSET $2",
            &[&limit, &offset],
        ).await.context("Failed to list users")?;

        rows.into_iter().map(|row| self.row_to_user(row)).collect()
    }

    /// List all users with organizations (with pagination)
    /// Returns the earliest organization created by each user (owner role) with spend limit
    /// Returns a tuple of (User, Option<UserOrganizationInfo>)
    pub async fn list_with_organizations(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<(User, Option<services::admin::UserOrganizationInfo>)>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
            SELECT DISTINCT ON (u.id)
                u.*,
                o.id as organization_id,
                o.name as organization_name,
                o.description as organization_description,
                olh.spend_limit as organization_spend_limit
            FROM users u
            LEFT JOIN organization_members om ON u.id = om.user_id and om.role = 'owner'
            LEFT JOIN organizations o ON om.organization_id = o.id AND o.is_active = true
            LEFT JOIN LATERAL (
                SELECT spend_limit
                FROM organization_limits_history
                WHERE organization_id = o.id
                  AND effective_until IS NULL
                ORDER BY effective_from DESC
                LIMIT 1
            ) olh ON true
            WHERE u.is_active = true
            ORDER BY u.id, o.created_at ASC NULLS LAST
            LIMIT $1
            OFFSET $2
            "#,
                &[&limit, &offset],
            )
            .await
            .context("Failed to list users with organizations")?;

        rows.into_iter()
            .map(|row| {
                let user = self.row_to_user(row.clone())?;
                let org_id: Option<Uuid> = row.get("organization_id");
                let org_name: Option<String> = row.get("organization_name");
                let org_description: Option<String> = row.get("organization_description");
                let spend_limit: Option<i64> = row.get("organization_spend_limit");

                let org_data = org_id.map(|id| services::admin::UserOrganizationInfo {
                    id,
                    name: org_name.unwrap_or_default(),
                    description: org_description,
                    spend_limit,
                });

                Ok((user, org_data))
            })
            .collect()
    }

    /// Search users by username or email
    pub async fn search(&self, query: &str, limit: i64) -> Result<Vec<User>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let pattern = format!("%{query}%");
        let rows = client.query(
            "SELECT * FROM users WHERE is_active = true AND (username ILIKE $1 OR email ILIKE $1) LIMIT $2",
            &[&pattern, &limit],
        ).await.context("Failed to search users")?;

        rows.into_iter().map(|row| self.row_to_user(row)).collect()
    }

    /// Deactivate a user (soft delete)
    pub async fn deactivate(&self, id: Uuid) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows_affected = client
            .execute("UPDATE users SET is_active = false WHERE id = $1", &[&id])
            .await
            .context("Failed to deactivate user")?;

        Ok(rows_affected > 0)
    }

    /// Update user's tokens_revoked_at timestamp
    pub async fn update_tokens_revoked_at(&self, id: Uuid) -> Result<()> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        client
            .execute(
                "UPDATE users SET tokens_revoked_at = NOW() WHERE id = $1",
                &[&id],
            )
            .await
            .context("Failed to update tokens_revoked_at")?;

        Ok(())
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
            tokens_revoked_at: row.get("tokens_revoked_at"),
        })
    }
}

// Convert database User to service User
fn db_user_to_service_user(db_user: User) -> services::auth::User {
    services::auth::User {
        id: services::auth::UserId(db_user.id),
        email: db_user.email,
        username: db_user.username,
        display_name: db_user.display_name,
        avatar_url: db_user.avatar_url,
        auth_provider: db_user.auth_provider,
        role: services::auth::UserRole::User, // TODO: Map from db_user if roles are added
        is_active: db_user.is_active,
        last_login: db_user.last_login_at,
        created_at: db_user.created_at,
        updated_at: db_user.updated_at,
        tokens_revoked_at: db_user.tokens_revoked_at,
    }
}

// Implement the service trait
#[async_trait]
impl services::auth::UserRepository for UserRepository {
    async fn create(
        &self,
        email: String,
        username: String,
        display_name: Option<String>,
        avatar_url: Option<String>,
    ) -> anyhow::Result<services::auth::User> {
        // For now, we'll use create_from_oauth with "manual" as the provider
        let db_user = self
            .create_from_oauth(
                email,
                username,
                display_name,
                avatar_url,
                "manual".to_string(),
                Uuid::new_v4().to_string(),
            )
            .await?;

        Ok(db_user_to_service_user(db_user))
    }

    async fn create_from_oauth(
        &self,
        email: String,
        username: String,
        display_name: Option<String>,
        avatar_url: Option<String>,
        auth_provider: String,
        provider_user_id: String,
    ) -> anyhow::Result<services::auth::User> {
        let db_user = self
            .create_from_oauth(
                email,
                username,
                display_name,
                avatar_url,
                auth_provider,
                provider_user_id,
            )
            .await?;

        Ok(db_user_to_service_user(db_user))
    }

    async fn get_by_id(
        &self,
        id: services::auth::UserId,
    ) -> anyhow::Result<Option<services::auth::User>> {
        let maybe_user = self.get_by_id(id.0).await?;
        Ok(maybe_user.map(db_user_to_service_user))
    }

    async fn get_by_email(&self, email: &str) -> anyhow::Result<Option<services::auth::User>> {
        let maybe_user = self.get_by_email(email).await?;
        Ok(maybe_user.map(db_user_to_service_user))
    }

    async fn update(
        &self,
        id: services::auth::UserId,
        display_name: Option<String>,
        avatar_url: Option<String>,
    ) -> anyhow::Result<Option<services::auth::User>> {
        match self.update_profile(id.0, display_name, avatar_url).await {
            Ok(user) => Ok(Some(db_user_to_service_user(user))),
            Err(e) => {
                // Check if error is because user wasn't found
                if e.to_string().contains("no rows") {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn update_last_login(&self, id: services::auth::UserId) -> anyhow::Result<()> {
        self.update_last_login(id.0).await
    }

    async fn update_tokens_revoked_at(&self, id: services::auth::UserId) -> anyhow::Result<()> {
        self.update_tokens_revoked_at(id.0).await
    }

    async fn delete(&self, id: services::auth::UserId) -> anyhow::Result<bool> {
        self.deactivate(id.0).await
    }

    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<services::auth::User>> {
        let db_users = self.list(limit, offset).await?;
        Ok(db_users.into_iter().map(db_user_to_service_user).collect())
    }
}
