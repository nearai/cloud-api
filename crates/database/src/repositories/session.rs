use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::{models::Session, retry_db};
use anyhow::{Context, Result};
use chrono::Utc;
use regex::Regex;
use services::common::RepositoryError;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use tracing::{debug, warn};
use uuid::Uuid;

pub struct SessionRepository {
    pool: DbPool,
}

// Covers the UI's 30-second upstream timeout plus network jitter.
const REFRESH_REUSE_WINDOW_SECONDS: i64 = 60;

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
        hex::encode(hasher.finalize())
    }

    /// Normalize User-Agent string by removing browser and platform versions.
    ///
    /// This removes product versions (e.g., "/129.0.6668.92") and OS versions
    /// (e.g., "iPhone OS 17_5" or "Android 14") so routine updates do not
    /// invalidate the session. Keep the platform and device names for binding.
    /// Examples:
    /// - "Chrome/129.0.6668.92" -> "Chrome"
    /// - "Safari/605.1.15" -> "Safari"
    /// - "Firefox/131.0" -> "Firefox"
    /// - "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36"
    ///   -> "Mozilla (Windows NT; Win64; x64) AppleWebKit (KHTML, like Gecko) Chrome Safari"
    fn normalize_user_agent(user_agent: &str) -> String {
        static VERSION_PATTERN: OnceLock<Regex> = OnceLock::new();
        static PLATFORM_VERSION_PATTERN: OnceLock<Regex> = OnceLock::new();

        let product_version = VERSION_PATTERN.get_or_init(|| {
            Regex::new(r"/[A-Za-z0-9._-]+").expect("Failed to compile version pattern regex")
        });
        let platform_version = PLATFORM_VERSION_PATTERN.get_or_init(|| {
            Regex::new(r"(?i)\b(OS|Android|Mac OS X|Windows NT)\s+\d+(?:[._]\d+)*")
                .expect("Failed to compile platform version pattern regex")
        });

        let without_product_versions = product_version.replace_all(user_agent, "");
        platform_version
            .replace_all(&without_product_versions, "$1")
            .trim()
            .to_string()
    }

    /// Create a new refresh token session
    pub async fn create(
        &self,
        user_id: Uuid,
        ip_address: Option<String>,
        user_agent: String,
        expires_in_hours: i64,
    ) -> Result<(Session, String)> {
        let id = Uuid::new_v4();
        let session_token = Self::generate_session_token();
        let token_hash = Self::hash_session_token(&session_token);

        // Normalize user agent to remove version numbers before storing
        let normalized_user_agent = Self::normalize_user_agent(&user_agent);

        let row = retry_db!("create_new_refresh_token", {
            let now = Utc::now();
            let expires_at = now
                + chrono::Duration::seconds(
                    expires_in_hours
                        .checked_mul(3600)
                        .context("Invalid expiration hours: value too large")
                        .map_err(RepositoryError::DataConversionError)?,
                );
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
            INSERT INTO refresh_tokens (
                id, user_id, token_hash, created_at, expires_at,
                ip_address, user_agent
            )
            SELECT $1, $2, $3, $4, $5, $6, $7
            WHERE EXISTS (SELECT 1 FROM users WHERE id = $2 AND is_active = true FOR SHARE)
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
                .map_err(map_db_error)?
                .ok_or_else(|| RepositoryError::NotFound("active user".to_string()))
        })?;

        debug!(
            "Created refresh token session: {} for user: {}",
            id, user_id
        );

        let session = self.row_to_session(row)?;
        Ok((session, session_token))
    }

    /// Validate a refresh token and return the associated session
    pub async fn validate(&self, session_token: &str, user_agent: &str) -> Result<Option<Session>> {
        // Hash the token directly (it already includes rt_ prefix if present)
        let token_hash = Self::hash_session_token(session_token);
        let now = Utc::now();

        // Normalize the incoming user agent
        let normalized_user_agent = Self::normalize_user_agent(user_agent);

        let row = retry_db!("validate_refresh_token", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
            SELECT * FROM refresh_tokens
            WHERE expires_at > $2
              AND (token_hash = $1 OR
                   (previous_token_hash = $1 AND rotated_at > $3))
            "#,
                    &[
                        &token_hash,
                        &now,
                        &(now - chrono::Duration::seconds(REFRESH_REUSE_WINDOW_SECONDS)),
                    ],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(row) => {
                let session = self.row_to_session(row)?;
                let stored_normalized = Self::normalize_user_agent(&session.user_agent);
                if stored_normalized == normalized_user_agent {
                    Ok(Some(session))
                } else {
                    warn!(session_id = %session.id, reason = "user_agent_mismatch", "Refresh token rejected");
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    /// Get a session by its session ID (not user ID)
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<Session>> {
        let row = retry_db!("get_session_by_id", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt("SELECT * FROM refresh_tokens WHERE id = $1", &[&id])
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(row) => Ok(Some(self.row_to_session(row)?)),
            None => Ok(None),
        }
    }

    /// List active refresh token sessions for a specific user (by user ID)
    pub async fn list_by_user(&self, user_id: Uuid) -> Result<Vec<Session>> {
        let rows = retry_db!("list_active_refresh_token_sessions_for_user", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
            SELECT * FROM refresh_tokens 
            WHERE user_id = $1 AND expires_at > $2
            ORDER BY created_at DESC
            "#,
                    &[&user_id, &Utc::now()],
                )
                .await
                .map_err(map_db_error)
        })?;

        rows.into_iter()
            .map(|row| self.row_to_session(row))
            .collect()
    }

    /// Extend a refresh token session's expiration time
    pub async fn extend(&self, session_id: Uuid, additional_hours: i64) -> Result<bool> {
        let new_expiry = Utc::now()
            + chrono::Duration::seconds(
                additional_hours
                    .checked_mul(3600)
                    .context("Invalid additional hours: value too large")?,
            );

        let result = retry_db!("extend_refresh_token_session", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    "UPDATE refresh_tokens SET expires_at = $1 WHERE id = $2",
                    &[&new_expiry, &session_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(result > 0)
    }

    /// Rotates a refresh token session.
    ///
    /// Serializes rotation per session. A duplicate use of the immediate
    /// predecessor within the reuse window returns the same successor; a request
    /// with the current token during that window does not rotate again. This
    /// prevents out-of-order HTTP responses from overwriting the browser's
    /// cookie with a superseded token.
    ///
    /// Callers must derive `successor_token` deterministically from `old_token`
    /// so concurrent retries present the same successor. Otherwise they fail
    /// instead of reusing the existing rotation.
    pub async fn rotate(
        &self,
        session_id: Uuid,
        old_token: &str,
        successor_token: &str,
        expires_in_hours: i64,
    ) -> Result<(Session, String)> {
        let old_token_hash = Self::hash_session_token(old_token);
        let successor_hash = Self::hash_session_token(successor_token);
        let new_expires_at = Utc::now()
            + chrono::Duration::seconds(
                expires_in_hours
                    .checked_mul(3600)
                    .context("Invalid expiration hours: value too large")?,
            );

        let (row, token, predecessor_reused) = retry_db!("rotate_refresh_token_session", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let tx = client.transaction().await.map_err(map_db_error)?;
            let row = tx
                .query_opt(
                    "SELECT * FROM refresh_tokens WHERE id = $1 AND expires_at > $2 FOR UPDATE",
                    &[&session_id, &Utc::now()],
                )
                .await
                .map_err(map_db_error)?
                .ok_or_else(|| {
                    RepositoryError::DatabaseError(anyhow::anyhow!(
                        "Token rotation failed: session not found"
                    ))
                })?;
            let current_hash: String = row.get("token_hash");
            let previous_hash: Option<String> = row.get("previous_token_hash");
            let rotated_at: Option<chrono::DateTime<Utc>> = row.get("rotated_at");
            let in_reuse_window = rotated_at.is_some_and(|at| {
                at > Utc::now() - chrono::Duration::seconds(REFRESH_REUSE_WINDOW_SECONDS)
            });

            let result = if current_hash == old_token_hash {
                if in_reuse_window {
                    // The current credential already refreshed recently.
                    (row, old_token.to_string(), false)
                } else {
                    let updated = tx
                        .query_one(
                            "UPDATE refresh_tokens SET token_hash = $1, previous_token_hash = $2, rotated_at = $3, expires_at = $4 WHERE id = $5 RETURNING *",
                            &[&successor_hash, &old_token_hash, &Utc::now(), &new_expires_at, &session_id],
                        )
                        .await
                        .map_err(map_db_error)?;
                    (updated, successor_token.to_string(), false)
                }
            } else if in_reuse_window
                && previous_hash.as_deref() == Some(old_token_hash.as_str())
                && current_hash == successor_hash
            {
                (row, successor_token.to_string(), true)
            } else {
                return Err(RepositoryError::DatabaseError(anyhow::anyhow!(
                    "Token rotation failed: token not found or already rotated"
                )));
            };
            tx.commit().await.map_err(map_db_error)?;
            Ok(result)
        })?;

        if predecessor_reused {
            warn!(session_id = %session_id, "Refresh token predecessor reused within grace window");
        } else {
            debug!(session_id = %session_id, "Refresh token session accepted");
        }

        let session = self.row_to_session(row)?;

        Ok((session, token))
    }

    /// Revoke a refresh token session
    pub async fn revoke(&self, session_id: Uuid) -> Result<bool> {
        let result = retry_db!("revoke_refresh_token_session", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute("DELETE FROM refresh_tokens WHERE id = $1", &[&session_id])
                .await
                .map_err(map_db_error)
        })?;

        Ok(result > 0)
    }

    /// Revoke all refresh token sessions for a user
    pub async fn revoke_all_for_user(&self, user_id: Uuid) -> Result<usize> {
        let result = retry_db!("revoke_all_refresh_token_sessions", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute("DELETE FROM refresh_tokens WHERE user_id = $1", &[&user_id])
                .await
                .map_err(map_db_error)
        })?;

        Ok(result as usize)
    }

    /// Clean up expired refresh token sessions
    pub async fn cleanup_expired(&self) -> Result<usize> {
        let result = retry_db!("clean_up_expried_refresh_token_session", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    "DELETE FROM refresh_tokens WHERE expires_at < $1",
                    &[&Utc::now()],
                )
                .await
                .map_err(map_db_error)
        })?;

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
        old_token: &str,
        successor_token: &str,
        expires_in_hours: i64,
    ) -> anyhow::Result<(services::auth::Session, String)> {
        let (db_session, token) = SessionRepository::rotate(
            self,
            session_id.0,
            old_token,
            successor_token,
            expires_in_hours,
        )
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

    async fn revoke_all_for_user(&self, user_id: services::auth::UserId) -> anyhow::Result<usize> {
        self.revoke_all_for_user(user_id.0).await
    }

    async fn cleanup_expired(&self) -> anyhow::Result<usize> {
        self.cleanup_expired().await
    }
}

#[cfg(test)]
mod tests {
    use super::SessionRepository;

    #[test]
    fn mobile_os_updates_keep_the_same_user_agent_binding() {
        let old_iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 Version/17.5 Mobile/15E148 Safari/604.1";
        let new_iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Version/18.0 Mobile/15E148 Safari/604.1";
        assert_eq!(
            SessionRepository::normalize_user_agent(old_iphone),
            SessionRepository::normalize_user_agent(new_iphone)
        );

        let old_android = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 Chrome/126.0.0.0 Mobile Safari/537.36";
        let new_android = "Mozilla/5.0 (Linux; Android 15; Pixel 8) AppleWebKit/537.36 Chrome/127.0.0.0 Mobile Safari/537.36";
        assert_eq!(
            SessionRepository::normalize_user_agent(old_android),
            SessionRepository::normalize_user_agent(new_android)
        );
        assert!(SessionRepository::normalize_user_agent(new_android).contains("Android; Pixel 8"));
    }

    #[test]
    fn different_device_still_fails_the_user_agent_binding() {
        let pixel_8 = "Mozilla/5.0 (Linux; Android 14; Pixel 8) Chrome/126.0.0.0";
        let pixel_9 = "Mozilla/5.0 (Linux; Android 15; Pixel 9) Chrome/127.0.0.0";
        assert_ne!(
            SessionRepository::normalize_user_agent(pixel_8),
            SessionRepository::normalize_user_agent(pixel_9)
        );
    }
}
