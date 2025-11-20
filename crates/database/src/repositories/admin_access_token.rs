use crate::models::AdminAccessToken;
use crate::pool::DbPool;
use anyhow::{Context, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};
use tracing::debug;
use uuid::Uuid;

pub struct AdminAccessTokenRepository {
    pool: DbPool,
}

impl AdminAccessTokenRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Generate a new admin access token
    fn generate_admin_access_token() -> String {
        format!("adm_{}", Uuid::new_v4().to_string().replace("-", ""))
    }

    /// Hash an admin access token for storage
    fn hash_admin_access_token(token: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Create a new admin access token
    pub async fn create(
        &self,
        created_by_user_id: Uuid,
        name: String,
        creation_reason: String,
        expires_at: chrono::DateTime<Utc>,
        user_agent: Option<String>,
    ) -> Result<(AdminAccessToken, String)> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let id = Uuid::new_v4();
        let admin_access_token = Self::generate_admin_access_token();
        let token_hash = Self::hash_admin_access_token(&admin_access_token);
        let now = Utc::now();

        let row = client
            .query_one(
                r#"
                INSERT INTO admin_access_token (
                    id, token_hash, created_by_user_id, name, creation_reason,
                    created_at, expires_at, is_active, user_agent
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                RETURNING *
                "#,
                &[
                    &id,
                    &token_hash,
                    &created_by_user_id,
                    &name,
                    &creation_reason,
                    &now,
                    &expires_at,
                    &true,
                    &user_agent,
                ],
            )
            .await
            .context("Failed to create admin access token")?;

        let admin_token = AdminAccessToken {
            id: row.get("id"),
            token_hash: row.get("token_hash"),
            created_by_user_id: row.get("created_by_user_id"),
            name: row.get("name"),
            creation_reason: row.get("creation_reason"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
            last_used_at: row.get("last_used_at"),
            is_active: row.get("is_active"),
            revoked_at: row.get("revoked_at"),
            revoked_by_user_id: row.get("revoked_by_user_id"),
            revocation_reason: row.get("revocation_reason"),
            user_agent: row.get("user_agent"),
        };

        debug!(
            "Created admin access token {} for user {}",
            name, created_by_user_id
        );

        Ok((admin_token, admin_access_token))
    }

    /// Validate an admin access token
    pub async fn validate(
        &self,
        token: &str,
        user_agent: Option<&str>,
    ) -> Result<Option<AdminAccessToken>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let token_hash = Self::hash_admin_access_token(token);
        let now = Utc::now();

        let row = client
            .query_opt(
                r#"
                SELECT * FROM admin_access_token 
                WHERE token_hash = $1 
                AND is_active = true 
                AND expires_at > $2
                AND (user_agent = $3 OR user_agent IS NULL)
                "#,
                &[&token_hash, &now, &user_agent],
            )
            .await
            .context("Failed to validate admin access token")?;

        match row {
            Some(row) => {
                // Update last_used_at
                if client
                    .execute(
                        "UPDATE admin_access_token SET last_used_at = $1 WHERE id = $2",
                        &[&now, &row.get::<_, Uuid>("id")],
                    )
                    .await
                    .is_err()
                {
                    // Log the error but don't fail the validation
                    // This is non-critical for token validation
                    tracing::warn!("Failed to update last_used_at for admin access token");
                }

                let admin_token = AdminAccessToken {
                    id: row.get("id"),
                    token_hash: row.get("token_hash"),
                    created_by_user_id: row.get("created_by_user_id"),
                    name: row.get("name"),
                    creation_reason: row.get("creation_reason"),
                    created_at: row.get("created_at"),
                    expires_at: row.get("expires_at"),
                    last_used_at: Some(now),
                    is_active: row.get("is_active"),
                    revoked_at: row.get("revoked_at"),
                    revoked_by_user_id: row.get("revoked_by_user_id"),
                    revocation_reason: row.get("revocation_reason"),
                    user_agent: row.get("user_agent"),
                };

                Ok(Some(admin_token))
            }
            None => Ok(None),
        }
    }

    /// Get admin access token by ID
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<AdminAccessToken>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt("SELECT * FROM admin_access_token WHERE id = $1", &[&id])
            .await
            .context("Failed to get admin access token by ID")?;

        match row {
            Some(row) => {
                let admin_token = AdminAccessToken {
                    id: row.get("id"),
                    token_hash: row.get("token_hash"),
                    created_by_user_id: row.get("created_by_user_id"),
                    name: row.get("name"),
                    creation_reason: row.get("creation_reason"),
                    created_at: row.get("created_at"),
                    expires_at: row.get("expires_at"),
                    last_used_at: row.get("last_used_at"),
                    is_active: row.get("is_active"),
                    revoked_at: row.get("revoked_at"),
                    revoked_by_user_id: row.get("revoked_by_user_id"),
                    revocation_reason: row.get("revocation_reason"),
                    user_agent: row.get("user_agent"),
                };
                Ok(Some(admin_token))
            }
            None => Ok(None),
        }
    }

    /// List admin access tokens with pagination
    pub async fn list(&self, limit: i64, offset: i64) -> Result<Vec<AdminAccessToken>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                "SELECT * FROM admin_access_token ORDER BY created_at DESC LIMIT $1 OFFSET $2",
                &[&limit, &offset],
            )
            .await
            .context("Failed to list admin access tokens")?;

        let mut admin_tokens = Vec::new();
        for row in rows {
            let admin_token = AdminAccessToken {
                id: row.get("id"),
                token_hash: row.get("token_hash"),
                created_by_user_id: row.get("created_by_user_id"),
                name: row.get("name"),
                creation_reason: row.get("creation_reason"),
                created_at: row.get("created_at"),
                expires_at: row.get("expires_at"),
                last_used_at: row.get("last_used_at"),
                is_active: row.get("is_active"),
                revoked_at: row.get("revoked_at"),
                revoked_by_user_id: row.get("revoked_by_user_id"),
                revocation_reason: row.get("revocation_reason"),
                user_agent: row.get("user_agent"),
            };
            admin_tokens.push(admin_token);
        }

        Ok(admin_tokens)
    }

    /// Revoke an admin access token
    pub async fn revoke(
        &self,
        id: Uuid,
        revoked_by_user_id: Uuid,
        revocation_reason: String,
    ) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();
        let result = client
            .execute(
                r#"
                UPDATE admin_access_token 
                SET is_active = false, revoked_at = $1, revoked_by_user_id = $2, revocation_reason = $3
                WHERE id = $4 AND is_active = true
                "#,
                &[&now, &revoked_by_user_id, &revocation_reason, &id],
            )
            .await
            .context("Failed to revoke admin access token")?;

        Ok(result > 0)
    }

    /// Count total admin access tokens
    pub async fn count(&self) -> Result<i64> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_one("SELECT COUNT(*) FROM admin_access_token", &[])
            .await
            .context("Failed to count admin access tokens")?;

        Ok(row.get(0))
    }

    /// Clean up expired tokens
    pub async fn cleanup_expired(&self) -> Result<usize> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();
        let result = client
            .execute(
                "UPDATE admin_access_token SET is_active = false WHERE expires_at <= $1 AND is_active = true",
                &[&now],
            )
            .await
            .context("Failed to cleanup expired admin access tokens")?;

        Ok(result as usize)
    }
}
