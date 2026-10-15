use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::{models::User, retry_db};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use services::auth::{AuthError, OAuthIdentity, OAuthUserInfo, SessionId, UserId};
use services::common::RepositoryError;
use tracing::debug;
use uuid::Uuid;

pub struct UserRepository {
    pool: DbPool,
}

fn oauth_identity_from_row(row: &tokio_postgres::Row) -> OAuthIdentity {
    OAuthIdentity {
        provider: row.get("provider"),
        provider_user_id: row.get("provider_user_id"),
        email: row.get("email"),
        email_verified: row.get("email_verified"),
        linked_at: row.get("linked_at"),
    }
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
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let tx = client.transaction().await.map_err(map_db_error)?;
            let row = tx
                .query_one(
                    r#"
            INSERT INTO users (
                id, email, username, display_name, avatar_url,
                created_at, updated_at, is_active,
                auth_provider, provider_user_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, true, $8, $9)
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
                .map_err(map_db_error)?;
            if auth_provider == "near" {
                tx.query_one("SELECT ensure_near_personal_organization($1)", &[&id])
                    .await
                    .map_err(map_db_error)?;
            }
            tx.commit().await.map_err(map_db_error)?;
            Ok(row)
        })?;

        debug!("Created/updated user: {}", id);
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

            client
                .query_opt(
                    "SELECT u.* FROM users u
                     WHERE u.auth_provider = $1 AND u.provider_user_id = $2 AND u.is_active = true
                     UNION ALL
                     SELECT u.* FROM user_oauth_identities i JOIN users u ON u.id = i.user_id
                     WHERE i.provider = $1 AND i.provider_user_id = $2 AND u.is_active = true
                     LIMIT 1",
                    &[&auth_provider, &provider_user_id],
                )
                .await
                .map_err(map_db_error)
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
                    "UPDATE users SET last_login_at = NOW() WHERE id = $1 AND is_active = true",
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

    fn escape_like_query(query: &str) -> String {
        query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
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

    /// List all users for admin views, including inactive users by default.
    pub async fn list_admin(
        &self,
        limit: i64,
        offset: i64,
        search: Option<String>,
        is_active: Option<bool>,
    ) -> Result<(Vec<User>, i64)> {
        let escaped_search = search.as_ref().map(|s| Self::escape_like_query(s));

        let total_count = retry_db!("count_admin_users", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let count_row = client
                .query_one(
                    r#"
            SELECT COUNT(*) as total_count
            FROM users
            WHERE ($1::BOOLEAN IS NULL OR is_active = $1)
              AND ($2::TEXT IS NULL
                   OR email ILIKE ('%' || $2 || '%') ESCAPE '\'
                   OR contact_email ILIKE ('%' || $2 || '%') ESCAPE '\'
                   OR username ILIKE ('%' || $2 || '%') ESCAPE '\'
                   OR display_name ILIKE ('%' || $2 || '%') ESCAPE '\'
                   OR id::TEXT ILIKE ('%' || $2 || '%') ESCAPE '\'
                   OR auth_provider ILIKE ('%' || $2 || '%') ESCAPE '\'
                   OR provider_user_id ILIKE ('%' || $2 || '%') ESCAPE '\')
            "#,
                    &[&is_active, &escaped_search],
                )
                .await
                .map_err(map_db_error)?;

            Ok(count_row.get::<_, i64>("total_count"))
        })?;

        let rows = retry_db!("list_admin_users_with_pagination", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
            SELECT *
            FROM users
            WHERE ($3::BOOLEAN IS NULL OR is_active = $3)
              AND ($4::TEXT IS NULL
                   OR email ILIKE ('%' || $4 || '%') ESCAPE '\'
                   OR contact_email ILIKE ('%' || $4 || '%') ESCAPE '\'
                   OR username ILIKE ('%' || $4 || '%') ESCAPE '\'
                   OR display_name ILIKE ('%' || $4 || '%') ESCAPE '\'
                   OR id::TEXT ILIKE ('%' || $4 || '%') ESCAPE '\'
                   OR auth_provider ILIKE ('%' || $4 || '%') ESCAPE '\'
                   OR provider_user_id ILIKE ('%' || $4 || '%') ESCAPE '\')
            ORDER BY created_at DESC
            LIMIT $1
            OFFSET $2
            "#,
                    &[&limit, &offset, &is_active, &escaped_search],
                )
                .await
                .map_err(map_db_error)
        })?;

        let users = rows
            .into_iter()
            .map(|row| self.row_to_user(row))
            .collect::<Result<Vec<_>>>()?;

        Ok((users, total_count))
    }

    /// List all users with organizations (with pagination)
    /// Returns the earliest organization created by each user (owner role) with spend limit and usage
    /// Returns a tuple of (User, Option<UserOrganizationInfo>)
    pub async fn list_with_organizations(
        &self,
        limit: i64,
        offset: i64,
        search: Option<String>,
        is_active: Option<bool>,
        search_by_name: Option<String>,
    ) -> Result<(
        Vec<(User, Option<services::admin::UserOrganizationInfo>)>,
        i64,
    )> {
        // Escape LIKE wildcard characters in user input to prevent injection
        let escaped_search = search.as_ref().map(|s| Self::escape_like_query(s));
        let escaped_org_search = search_by_name.as_ref().map(|s| Self::escape_like_query(s));

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
            WHERE ($1::BOOLEAN IS NULL OR u.is_active = $1)
              AND ($2::TEXT IS NULL
                   OR u.email ILIKE ('%' || $2 || '%') ESCAPE '\'
                   OR u.contact_email ILIKE ('%' || $2 || '%') ESCAPE '\'
                   OR u.username ILIKE ('%' || $2 || '%') ESCAPE '\'
                   OR u.display_name ILIKE ('%' || $2 || '%') ESCAPE '\'
                   OR u.id::TEXT ILIKE ('%' || $2 || '%') ESCAPE '\'
                   OR u.auth_provider ILIKE ('%' || $2 || '%') ESCAPE '\'
                   OR u.provider_user_id ILIKE ('%' || $2 || '%') ESCAPE '\')
              AND ($3::TEXT IS NULL
                   OR o.name ILIKE ('%' || $3 || '%') ESCAPE '\')
            "#,
                    &[&is_active, &escaped_search, &escaped_org_search],
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
            WITH first_org_per_user AS (
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
                    SELECT SUM(spend_limit)::BIGINT AS spend_limit
                    FROM organization_limits_history
                    WHERE organization_id = o.id
                      AND effective_until IS NULL
                ) olh ON true
                LEFT JOIN organization_balance ob ON o.id = ob.organization_id
                WHERE ($3::BOOLEAN IS NULL OR u.is_active = $3)
                  AND ($4::TEXT IS NULL
                       OR u.email ILIKE ('%' || $4 || '%') ESCAPE '\'
                       OR u.contact_email ILIKE ('%' || $4 || '%') ESCAPE '\'
                       OR u.username ILIKE ('%' || $4 || '%') ESCAPE '\'
                       OR u.display_name ILIKE ('%' || $4 || '%') ESCAPE '\'
                       OR u.id::TEXT ILIKE ('%' || $4 || '%') ESCAPE '\'
                       OR u.auth_provider ILIKE ('%' || $4 || '%') ESCAPE '\'
                       OR u.provider_user_id ILIKE ('%' || $4 || '%') ESCAPE '\')
                  AND ($5::TEXT IS NULL
                       OR o.name ILIKE ('%' || $5 || '%') ESCAPE '\')
                ORDER BY u.id, o.created_at ASC NULLS LAST
            )
            SELECT *
            FROM first_org_per_user
            ORDER BY created_at DESC
            LIMIT $1
            OFFSET $2
            "#,
                    &[
                        &limit,
                        &offset,
                        &is_active,
                        &escaped_search,
                        &escaped_org_search,
                    ],
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
            contact_email: row.get("contact_email"),
            contact_email_verified_at: row.get("contact_email_verified_at"),
            email_opt_in_at: row.get("email_opt_in_at"),
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
        provider_user_id: db_user.provider_user_id,
        role: services::auth::UserRole::User, // TODO: Map from db_user if roles are added
        is_active: db_user.is_active,
        last_login: db_user.last_login_at,
        created_at: db_user.created_at,
        updated_at: db_user.updated_at,
        tokens_revoked_at: db_user.tokens_revoked_at,
        contact_email: db_user.contact_email,
        contact_email_verified_at: db_user.contact_email_verified_at,
        email_opt_in_at: db_user.email_opt_in_at,
    }
}

// Implement the service trait
#[async_trait]
impl services::auth::UserRepository for UserRepository {
    async fn try_link_oauth_identity_by_email(
        &self,
        info: &OAuthUserInfo,
    ) -> anyhow::Result<Option<services::auth::User>> {
        if !info.email_verified || !matches!(info.provider.as_str(), "google" | "github") {
            return Ok(None);
        }
        let client = self.pool.get().await?;
        // Lock the matching account while attaching the identity so its email
        // cannot change between the match and insert. Never replace an identity.
        client.execute(
            "WITH existing_user AS (
                SELECT id FROM users WHERE email=$1 AND is_active=true
                AND auth_provider IN ('google','github') AND auth_provider<>$2 FOR UPDATE
             )
             INSERT INTO user_oauth_identities (user_id,provider,provider_user_id,email,email_verified)
             SELECT id,$2,$3,$1,true FROM existing_user ON CONFLICT DO NOTHING",
            &[&info.email, &info.provider, &info.provider_user_id],
        ).await.map_err(map_db_error)?;
        drop(client);
        // A concurrent login/link may already own this stable provider identity.
        // Resolve its actual owner rather than treating DO NOTHING as a bind.
        Ok(self
            .get_by_provider(&info.provider, &info.provider_user_id)
            .await?
            .map(db_user_to_service_user))
    }

    async fn observe_oauth_identity(
        &self,
        user_id: UserId,
        info: &OAuthUserInfo,
    ) -> anyhow::Result<()> {
        if !matches!(info.provider.as_str(), "google" | "github") {
            return Ok(());
        }
        let client = self.pool.get().await?;
        let count = client
            .execute(
                "UPDATE user_oauth_identities SET email=$4, email_verified=$5
             WHERE user_id=$1 AND provider=$2 AND provider_user_id=$3",
                &[
                    &user_id.0,
                    &info.provider,
                    &info.provider_user_id,
                    &info.email,
                    &info.email_verified,
                ],
            )
            .await
            .map_err(map_db_error)?;
        anyhow::ensure!(count == 1, "OAuth identity does not belong to this account");
        Ok(())
    }

    async fn list_oauth_identities(&self, user_id: UserId) -> anyhow::Result<Vec<OAuthIdentity>> {
        let client = self.pool.get().await?;
        let rows = client.query("SELECT provider, provider_user_id, email, email_verified, linked_at FROM user_oauth_identities WHERE user_id=$1 ORDER BY provider", &[&user_id.0]).await?;
        Ok(rows.iter().map(oauth_identity_from_row).collect())
    }

    async fn store_oauth_link_request(
        &self,
        user_id: UserId,
        session_id: SessionId,
        code_hash: &str,
        info: &OAuthUserInfo,
    ) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "DELETE FROM oauth_link_requests WHERE expires_at <= NOW()",
                &[],
            )
            .await?;
        client.execute("INSERT INTO oauth_link_requests (code_hash,user_id,session_id,provider,provider_user_id,email,email_verified,expires_at)
            VALUES ($1,$2,$3,$4,$5,$6,$7,NOW()+INTERVAL '5 minutes')",
            &[&code_hash,&user_id.0,&session_id.0,&info.provider,&info.provider_user_id,&info.email,&info.email_verified]).await?;
        Ok(())
    }

    async fn complete_oauth_link_request(
        &self,
        user_id: UserId,
        session_id: SessionId,
        code_hash: &str,
    ) -> Result<OAuthIdentity, AuthError> {
        async fn complete(
            repo: &UserRepository,
            user_id: Uuid,
            session_id: Uuid,
            code_hash: &str,
        ) -> anyhow::Result<OAuthIdentity> {
            let mut client = repo.pool.get().await?;
            let tx = client.transaction().await?;
            // Lock the account and original session so logout/account revocation
            // cannot interleave with the final identity mutation.
            let active = tx
                .query_opt(
                    "SELECT u.id FROM users u JOIN refresh_tokens r ON r.user_id=u.id
                WHERE u.id=$1 AND r.id=$2 AND u.is_active=true
                AND u.auth_provider IN ('google','github') AND r.expires_at>NOW()
                AND (u.tokens_revoked_at IS NULL OR r.created_at>u.tokens_revoked_at)
                FOR UPDATE OF u,r",
                    &[&user_id, &session_id],
                )
                .await?;
            if active.is_none() {
                return Err(AuthError::OAuthLinkInvalid.into());
            }
            let request = tx
                .query_opt(
                    "DELETE FROM oauth_link_requests
                WHERE code_hash=$1 AND user_id=$2 AND session_id=$3 AND expires_at>NOW()
                RETURNING provider,provider_user_id,email,email_verified",
                    &[&code_hash, &user_id, &session_id],
                )
                .await?
                .ok_or(AuthError::OAuthLinkInvalid)?;
            let provider: String = request.get("provider");
            let provider_user_id: String = request.get("provider_user_id");
            let email: String = request.get("email");
            let verified: bool = request.get("email_verified");
            if !verified {
                return Err(AuthError::OAuthEmailUnverified.into());
            }
            let existing = tx
                .query_opt(
                    "SELECT user_id,provider_user_id FROM user_oauth_identities
                WHERE (provider=$1 AND provider_user_id=$2) OR (user_id=$3 AND provider=$1)
                LIMIT 1",
                    &[&provider, &provider_user_id, &user_id],
                )
                .await?;
            if let Some(existing) = existing {
                if existing.get::<_, Uuid>("user_id") != user_id
                    || existing.get::<_, String>("provider_user_id") != provider_user_id
                {
                    return Err(AuthError::OAuthLinkConflict.into());
                }
            }
            tx.execute("INSERT INTO user_oauth_identities (user_id,provider,provider_user_id,email,email_verified)
                VALUES ($1,$2,$3,$4,$5) ON CONFLICT (provider,provider_user_id) DO NOTHING",
                &[&user_id,&provider,&provider_user_id,&email,&verified]).await.map_err(map_db_error)?;
            // A concurrent account may have inserted this provider identity.
            // Never treat DO NOTHING as a successful bind without checking owner.
            let row = tx
                .query_one(
                    "SELECT user_id,provider,provider_user_id,email,email_verified,linked_at
                FROM user_oauth_identities WHERE provider=$1 AND provider_user_id=$2",
                    &[&provider, &provider_user_id],
                )
                .await?;
            if row.get::<_, Uuid>("user_id") != user_id {
                return Err(AuthError::OAuthLinkConflict.into());
            }
            let identity = oauth_identity_from_row(&row);
            tx.commit().await?;
            Ok(identity)
        }
        complete(self, user_id.0, session_id.0, code_hash)
            .await
            .map_err(|e| {
                if matches!(
                    e.downcast_ref::<RepositoryError>(),
                    Some(RepositoryError::AlreadyExists)
                ) {
                    AuthError::OAuthLinkConflict
                } else {
                    match e.downcast::<AuthError>() {
                        Ok(error) => error,
                        Err(error) => {
                            AuthError::InternalError(format!("Failed to link account: {error}"))
                        }
                    }
                }
            })
    }
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

    async fn get_by_provider(
        &self,
        auth_provider: &str,
        provider_user_id: &str,
    ) -> anyhow::Result<Option<services::auth::User>> {
        let maybe_user = self
            .get_by_provider(auth_provider, provider_user_id)
            .await?;
        Ok(maybe_user.map(db_user_to_service_user))
    }

    async fn update_email(&self, id: services::auth::UserId, email: String) -> anyhow::Result<()> {
        retry_db!("update_user_email", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    "UPDATE users SET email = $2, updated_at = NOW() WHERE id = $1 AND is_active = true",
                    &[&id.0, &email],
                )
                .await
                .map_err(map_db_error)
        })?;
        Ok(())
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
