use crate::models::Session;
use crate::pool::DbPool;
use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use tracing::debug;
use uuid::Uuid;

pub struct SessionRepository {
    pool: DbPool,
}

impl SessionRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Generate a new refresh token
    fn generate_session_token() -> String {
        format!("rt_{}", Uuid::new_v4().to_string().replace("-", ""))
    }

    /// Hash a refresh token for storage
    fn hash_session_token(token: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Normalize User-Agent string by removing version numbers.
    ///
    /// This removes version numbers (e.g., "/129.0.6668.92") to prevent
    /// session invalidation when browsers update. Examples:
    /// - "Chrome/129.0.6668.92" -> "Chrome"
    /// - "Safari/605.1.15" -> "Safari"
    /// - "Firefox/131.0" -> "Firefox"
    /// - "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36"
    ///   -> "Mozilla (Windows NT 10.0; Win64; x64) AppleWebKit (KHTML, like Gecko) Chrome Safari"
    fn normalize_user_agent(user_agent: &str) -> String {
        // Remove version patterns: "/" followed by digits and dots
        // This matches patterns like "/129.0.6668.92", "/605.1.15", "/131.0", "/537.36"
        static VERSION_PATTERN: OnceLock<Regex> = OnceLock::new();

        let pattern = VERSION_PATTERN.get_or_init(|| {
            Regex::new(r"/[\d.]+").expect("Failed to compile version pattern regex")
        });

        pattern.replace_all(user_agent, "").trim().to_string()
    }

    /// Create a new refresh token session
    pub async fn create(
        &self,
        user_id: Uuid,
        ip_address: Option<String>,
        user_agent: String,
        expires_in_hours: i64,
    ) -> Result<(Session, String)> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let id = Uuid::new_v4();
        let session_token = Self::generate_session_token();
        let token_hash = Self::hash_session_token(&session_token);
        let now = Utc::now();
        let expires_at = now + Duration::hours(expires_in_hours);

        // Normalize user agent to remove version numbers before storing
        let normalized_user_agent = Self::normalize_user_agent(&user_agent);

        let row = client
            .query_one(
                r#"
            INSERT INTO refresh_tokens (
                id, user_id, token_hash, created_at, expires_at,
                ip_address, user_agent
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING *
            "#,
                &[
                    &id,
                    &user_id,
                    &token_hash,
                    &now,
                    &expires_at,
                    &ip_address,
                    &normalized_user_agent,
                ],
            )
            .await
            .context("Failed to create refresh token session")?;

        debug!(
            "Created refresh token session: {} for user: {}",
            id, user_id
        );

        let session = self.row_to_session(row)?;
        Ok((session, session_token))
    }

    /// Validate a refresh token and return the associated session
    pub async fn validate(&self, session_token: &str, user_agent: &str) -> Result<Option<Session>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        // Hash the token directly (it already includes rt_ prefix if present)
        let token_hash = Self::hash_session_token(session_token);
        let now = Utc::now();

        // Normalize the incoming user agent
        let normalized_user_agent = Self::normalize_user_agent(user_agent);

        let row = client
            .query_opt(
                r#"
            SELECT * FROM refresh_tokens 
            WHERE token_hash = $1 AND expires_at > $2
            "#,
                &[&token_hash, &now],
            )
            .await
            .context("Failed to validate refresh token")?;

        match row {
            Some(row) => {
                let session = self.row_to_session(row)?;
                let stored_normalized = Self::normalize_user_agent(&session.user_agent);
                if stored_normalized == normalized_user_agent {
                    Ok(Some(session))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    /// Get a session by its session ID (not user ID)
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<Session>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt("SELECT * FROM refresh_tokens WHERE id = $1", &[&id])
            .await
            .context("Failed to query refresh token session")?;

        match row {
            Some(row) => Ok(Some(self.row_to_session(row)?)),
            None => Ok(None),
        }
    }

    /// List active refresh token sessions for a specific user (by user ID)
    pub async fn list_by_user(&self, user_id: Uuid) -> Result<Vec<Session>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
            SELECT * FROM refresh_tokens 
            WHERE user_id = $1 AND expires_at > $2
            ORDER BY created_at DESC
            "#,
                &[&user_id, &Utc::now()],
            )
            .await
            .context("Failed to list user refresh token sessions")?;

        rows.into_iter()
            .map(|row| self.row_to_session(row))
            .collect()
    }

    /// Extend a refresh token session's expiration time
    pub async fn extend(&self, session_id: Uuid, additional_hours: i64) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let new_expiry = Utc::now() + Duration::hours(additional_hours);

        let result = client
            .execute(
                "UPDATE refresh_tokens SET expires_at = $1 WHERE id = $2",
                &[&new_expiry, &session_id],
            )
            .await
            .context("Failed to extend refresh token session")?;

        Ok(result > 0)
    }

    /// Rotates a refresh token session.
    ///
    /// This operation atomically updates the token hash and expiration time in the database,
    /// invalidating the old token. This ensures that the previous token can no longer be used.
    ///
    /// The old_token_hash is included in the WHERE clause to prevent race conditions where
    /// two requests try to rotate the same token simultaneously. If the token was already
    /// rotated, no row will be updated and an error will be returned.
    ///
    /// Returns the updated session and the new plaintext token.
    pub async fn rotate(
        &self,
        session_id: Uuid,
        old_token_hash: &str,
        expires_in_hours: i64,
    ) -> Result<(Session, String)> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let new_session_token = Self::generate_session_token();
        let new_token_hash = Self::hash_session_token(&new_session_token);
        let new_expires_at = Utc::now() + Duration::hours(expires_in_hours);

        let row = client
            .query_opt(
                r#"
                UPDATE refresh_tokens
                SET token_hash = $1, expires_at = $2
                WHERE id = $3 AND token_hash = $4
                RETURNING *
                "#,
                &[
                    &new_token_hash,
                    &new_expires_at,
                    &session_id,
                    &old_token_hash,
                ],
            )
            .await
            .context("Failed to rotate refresh token session")?;

        let row = row.ok_or_else(|| {
            anyhow::anyhow!("Token rotation failed: token not found or already rotated")
        })?;

        debug!("Rotated refresh token session: {session_id}",);

        let session = self.row_to_session(row)?;

        Ok((session, new_session_token))
    }

    /// Revoke a refresh token session
    pub async fn revoke(&self, session_id: Uuid) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let result = client
            .execute("DELETE FROM refresh_tokens WHERE id = $1", &[&session_id])
            .await
            .context("Failed to revoke refresh token session")?;

        Ok(result > 0)
    }

    /// Revoke all refresh token sessions for a user
    pub async fn revoke_all_for_user(&self, user_id: Uuid) -> Result<usize> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let result = client
            .execute("DELETE FROM refresh_tokens WHERE user_id = $1", &[&user_id])
            .await
            .context("Failed to revoke user refresh token sessions")?;

        Ok(result as usize)
    }

    /// Clean up expired refresh token sessions
    pub async fn cleanup_expired(&self) -> Result<usize> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let result = client
            .execute(
                "DELETE FROM refresh_tokens WHERE expires_at < $1",
                &[&Utc::now()],
            )
            .await
            .context("Failed to cleanup expired refresh token sessions")?;

        debug!("Cleaned up {} expired refresh token sessions", result);
        Ok(result as usize)
    }

    // Helper function to convert database row to Session
    fn row_to_session(&self, row: tokio_postgres::Row) -> Result<Session> {
        Ok(Session {
            id: row.get("id"),
            user_id: row.get("user_id"),
            token_hash: row.get("token_hash"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
            ip_address: row.get("ip_address"),
            user_agent: row.get("user_agent"),
        })
    }
}

// Implement the service trait
#[async_trait::async_trait]
impl services::auth::SessionRepository for SessionRepository {
    async fn create(
        &self,
        user_id: services::auth::UserId,
        ip_address: Option<String>,
        user_agent: String,
        expires_in_hours: i64,
    ) -> anyhow::Result<(services::auth::Session, String)> {
        let (db_session, token) = self
            .create(user_id.0, ip_address, user_agent, expires_in_hours)
            .await?;

        let service_session = services::auth::Session {
            id: services::auth::SessionId(db_session.id),
            user_id: services::auth::UserId(db_session.user_id),
            token_hash: db_session.token_hash,
            created_at: db_session.created_at,
            expires_at: db_session.expires_at,
            ip_address: db_session.ip_address,
            user_agent: db_session.user_agent,
        };

        Ok((service_session, token))
    }

    async fn validate(
        &self,
        session_token: services::auth::SessionToken,
        user_agent: &str,
    ) -> anyhow::Result<Option<services::auth::Session>> {
        let maybe_session = self.validate(&session_token.0, user_agent).await?;

        Ok(maybe_session.map(|db_session| services::auth::Session {
            id: services::auth::SessionId(db_session.id),
            user_id: services::auth::UserId(db_session.user_id),
            token_hash: db_session.token_hash,
            created_at: db_session.created_at,
            expires_at: db_session.expires_at,
            ip_address: db_session.ip_address,
            user_agent: db_session.user_agent,
        }))
    }

    async fn get_by_id(
        &self,
        session_id: services::auth::SessionId,
    ) -> anyhow::Result<Option<services::auth::Session>> {
        let maybe_session = self.get_by_id(session_id.0).await?;

        Ok(maybe_session.map(|db_session| services::auth::Session {
            id: services::auth::SessionId(db_session.id),
            user_id: services::auth::UserId(db_session.user_id),
            token_hash: db_session.token_hash,
            created_at: db_session.created_at,
            expires_at: db_session.expires_at,
            ip_address: db_session.ip_address,
            user_agent: db_session.user_agent,
        }))
    }

    async fn list_by_user(
        &self,
        user_id: services::auth::UserId,
    ) -> anyhow::Result<Vec<services::auth::Session>> {
        let db_sessions = self.list_by_user(user_id.0).await?;

        Ok(db_sessions
            .into_iter()
            .map(|db_session| services::auth::Session {
                id: services::auth::SessionId(db_session.id),
                user_id: services::auth::UserId(db_session.user_id),
                token_hash: db_session.token_hash,
                created_at: db_session.created_at,
                expires_at: db_session.expires_at,
                ip_address: db_session.ip_address,
                user_agent: db_session.user_agent,
            })
            .collect())
    }

    async fn extend(
        &self,
        session_id: services::auth::SessionId,
        additional_hours: i64,
    ) -> anyhow::Result<bool> {
        self.extend(session_id.0, additional_hours).await
    }

    async fn revoke(&self, session_id: services::auth::SessionId) -> anyhow::Result<bool> {
        self.revoke(session_id.0).await
    }

    async fn rotate(
        &self,
        session_id: services::auth::SessionId,
        old_token_hash: &str,
        expires_in_hours: i64,
    ) -> anyhow::Result<(services::auth::Session, String)> {
        let (db_session, token) =
            SessionRepository::rotate(self, session_id.0, old_token_hash, expires_in_hours).await?;

        let service_session = services::auth::Session {
            id: services::auth::SessionId(db_session.id),
            user_id: services::auth::UserId(db_session.user_id),
            token_hash: db_session.token_hash,
            created_at: db_session.created_at,
            expires_at: db_session.expires_at,
            ip_address: db_session.ip_address,
            user_agent: db_session.user_agent,
        };

        Ok((service_session, token))
    }

    async fn revoke_all_for_user(&self, user_id: services::auth::UserId) -> anyhow::Result<usize> {
        self.revoke_all_for_user(user_id.0).await
    }

    async fn cleanup_expired(&self) -> anyhow::Result<usize> {
        self.cleanup_expired().await
    }
}
