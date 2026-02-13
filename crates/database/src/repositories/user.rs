use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::{models::User, retry_db};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use services::common::RepositoryError;
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
        let id = Uuid::new_v4();

        let row = retry_db!("create_new_user", {
            let now = Utc::now();
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
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
                .map_err(map_db_error)
        })?;

        debug!("Created/updated user: {} ({})", email, id);
        self.row_to_user(row)
    }

    /// Get a user by ID
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<User>> {
        let row = retry_db!("get_user_by_id", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT * FROM users WHERE id = $1 AND is_active = true",
                    &[&id],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(row) => Ok(Some(self.row_to_user(row)?)),
            None => Ok(None),
        }
    }

    /// Get a user by email
    pub async fn get_by_email(&self, email: &str) -> Result<Option<User>> {
        let row = retry_db!("get_user_by_email", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT * FROM users WHERE email = $1 AND is_active = true",
                    &[&email],
                )
                .await
                .map_err(map_db_error)
        })?;

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
        let row = retry_db!("get_user_by_oauth_details", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client.query_opt(
            "SELECT * FROM users WHERE auth_provider = $1 AND provider_user_id = $2 AND is_active = true",
            &[&auth_provider, &provider_user_id],
        ).await.map_err(map_db_error)
        })?;

        match row {
            Some(row) => Ok(Some(self.row_to_user(row)?)),
            None => Ok(None),
        }
    }

    /// Update user's last login time
    pub async fn update_last_login(&self, id: Uuid) -> Result<()> {
        retry_db!("update_user_last_login_time", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    "UPDATE users SET last_login_at = NOW() WHERE id = $1",
                    &[&id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(())
    }

    /// Update user profile
    pub async fn update_profile(
        &self,
        id: Uuid,
        display_name: Option<String>,
        avatar_url: Option<String>,
    ) -> Result<User> {
        let row = retry_db!("update_user_profile", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
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
                .map_err(map_db_error)
        })?;

        debug!("Updated profile for user: {}", id);
        self.row_to_user(row)
    }

    /// Get the number of active users
    pub async fn get_active_user_count(&self) -> Result<i64> {
        let row = retry_db!("get_number_of_active_users", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                SELECT COUNT(*) as count FROM users WHERE is_active = true
                "#,
                    &[],
                )
                .await
                .map_err(map_db_error)
        })?;
        Ok(row.get::<_, i64>("count"))
    }

    /// List all users (with pagination)
    pub async fn list(&self, limit: i64, offset: i64) -> Result<Vec<User>> {
        let rows = retry_db!("list_all_users_with_pagination", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client.query(
            "SELECT * FROM users WHERE is_active = true ORDER BY created_at DESC LIMIT $1 OFFSET $2",
            &[&limit, &offset],
        ).await.map_err(map_db_error)
        })?;

        rows.into_iter().map(|row| self.row_to_user(row)).collect()
    }

    /// List all users with organizations (with pagination)
    /// Returns the earliest organization created by each user (owner role) with spend limit and usage
    /// Returns a tuple of (User, Option<UserOrganizationInfo>)
    pub async fn list_with_organizations(
        &self,
        limit: i64,
        offset: i64,
        search_by_name: Option<String>,
    ) -> Result<(
        Vec<(User, Option<services::admin::UserOrganizationInfo>)>,
        i64,
    )> {
        // Escape LIKE wildcard characters in user input to prevent injection
        let escaped_search = search_by_name.as_ref().map(|s| {
            s.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        });

        // Get total count of matching users (independent of pagination)
        let total_count = retry_db!("count_users_with_organizations", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let count_row = client
                .query_one(
                    r#"
            SELECT COUNT(DISTINCT u.id) as total_count
            FROM users u
            LEFT JOIN organization_members om ON u.id = om.user_id AND om.role = 'owner'
            LEFT JOIN organizations o ON om.organization_id = o.id AND o.is_active = true
            WHERE u.is_active = true
              AND ($1::TEXT IS NULL
                   OR o.name ILIKE ('%' || $1 || '%') ESCAPE '\'
                   OR o.id IS NULL)
            "#,
                    &[&escaped_search],
                )
                .await
                .map_err(map_db_error)?;

            Ok(count_row.get::<_, i64>("total_count"))
        })?;

        // Get paginated results with organization info
        let rows = retry_db!("list_all_users_with_organizations_with_pagination", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
            SELECT DISTINCT ON (u.id)
                u.*,
                o.id as organization_id,
                o.name as organization_name,
                o.description as organization_description,
                olh.spend_limit as organization_spend_limit,
                ob.total_spent as organization_total_spent,
                ob.total_requests as organization_total_requests,
                ob.total_tokens as organization_total_tokens
            FROM users u
            LEFT JOIN organization_members om ON u.id = om.user_id AND om.role = 'owner'
            LEFT JOIN organizations o ON om.organization_id = o.id AND o.is_active = true
            LEFT JOIN LATERAL (
                SELECT COALESCE(SUM(spend_limit), 0)::BIGINT AS spend_limit
                FROM organization_limits_history
                WHERE organization_id = o.id
                  AND effective_until IS NULL
            ) olh ON true
            LEFT JOIN organization_balance ob ON o.id = ob.organization_id
            WHERE u.is_active = true
              AND ($3::TEXT IS NULL 
                   OR o.name ILIKE ('%' || $3 || '%') ESCAPE '\'
                   OR o.id IS NULL)
            ORDER BY u.id, o.created_at ASC NULLS LAST
            LIMIT $1
            OFFSET $2
            "#,
                    &[&limit, &offset, &escaped_search],
                )
                .await
                .map_err(map_db_error)
        })?;

        let result = rows
            .into_iter()
            .map(|row| {
                let user = self.row_to_user(row.clone())?;
                let org_id: Option<Uuid> = row.get("organization_id");
                let org_name: Option<String> = row.get("organization_name");
                let org_description: Option<String> = row.get("organization_description");
                let spend_limit: Option<i64> = row.get("organization_spend_limit");
                let total_spent: Option<i64> = row.get("organization_total_spent");
                let total_requests: Option<i64> = row.get("organization_total_requests");
                let total_tokens: Option<i64> = row.get("organization_total_tokens");

                let org_data = org_id.map(|id| services::admin::UserOrganizationInfo {
                    id,
                    name: org_name.unwrap_or_default(),
                    description: org_description,
                    spend_limit,
                    total_spent,
                    total_requests,
                    total_tokens,
                });

                Ok((user, org_data))
            })
            .collect::<Result<Vec<_>>>()?;

        Ok((result, total_count))
    }

    /// Search users by username or email
    pub async fn search(&self, query: &str, limit: i64) -> Result<Vec<User>> {
        let pattern = format!("%{query}%");
        let rows = retry_db!("search_users_by_username_or_email", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client.query(
            "SELECT * FROM users WHERE is_active = true AND (username ILIKE $1 OR email ILIKE $1) LIMIT $2",
            &[&pattern, &limit],
        ).await.map_err(map_db_error)
        })?;

        rows.into_iter().map(|row| self.row_to_user(row)).collect()
    }

    /// Deactivate a user (soft delete)
    pub async fn deactivate(&self, id: Uuid) -> Result<bool> {
        let rows_affected = retry_db!("deactivate_user_soft_delete", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute("UPDATE users SET is_active = false WHERE id = $1", &[&id])
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows_affected > 0)
    }

    /// Update user's tokens_revoked_at timestamp
    pub async fn update_tokens_revoked_at(&self, id: Uuid) -> Result<()> {
        retry_db!("update_user_token_revoked_at_timestamp", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    "UPDATE users SET tokens_revoked_at = NOW() WHERE id = $1",
                    &[&id],
                )
                .await
                .map_err(map_db_error)
        })?;

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
