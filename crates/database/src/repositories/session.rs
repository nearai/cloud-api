use crate::models::Session;
use crate::pool::DbPool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use sha2::{Digest, Sha256};
use tracing::debug;
use uuid::Uuid;

pub struct SessionRepository {
    pool: DbPool,
}

impl SessionRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Generate a new session token
    fn generate_session_token() -> String {
        format!("sess_{}", Uuid::new_v4().to_string().replace("-", ""))
    }

    /// Hash a session token for storage
    fn hash_session_token(token: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Create a new session
    pub async fn create(
        &self,
        user_id: Uuid,
        ip_address: Option<String>,
        user_agent: Option<String>,
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

        let row = client
            .query_one(
                r#"
            INSERT INTO sessions (
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
                    &user_agent,
                ],
            )
            .await
            .context("Failed to create session")?;

        debug!("Created session: {} for user: {}", id, user_id);

        let session = self.row_to_session(row)?;
        Ok((session, session_token))
    }

    /// Validate a session token and return the associated session
    pub async fn validate(&self, session_token: Uuid) -> Result<Option<Session>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();

        let row = client
            .query_opt(
                r#"
            SELECT * FROM sessions 
            WHERE id = $1 AND expires_at > $2
            "#,
                &[&session_token, &now],
            )
            .await
            .context("Failed to validate session")?;

        match row {
            Some(row) => Ok(Some(self.row_to_session(row)?)),
            None => Ok(None),
        }
    }

    /// Get a session by ID
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<Session>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt("SELECT * FROM sessions WHERE id = $1", &[&id])
            .await
            .context("Failed to query session")?;

        match row {
            Some(row) => Ok(Some(self.row_to_session(row)?)),
            None => Ok(None),
        }
    }

    /// List active sessions for a user
    pub async fn list_by_user(&self, user_id: Uuid) -> Result<Vec<Session>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
            SELECT * FROM sessions 
            WHERE user_id = $1 AND expires_at > $2
            ORDER BY created_at DESC
            "#,
                &[&user_id, &Utc::now()],
            )
            .await
            .context("Failed to list user sessions")?;

        rows.into_iter()
            .map(|row| self.row_to_session(row))
            .collect()
    }

    /// Extend a session's expiration time
    pub async fn extend(&self, session_id: Uuid, additional_hours: i64) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let new_expiry = Utc::now() + Duration::hours(additional_hours);

        let result = client
            .execute(
                "UPDATE sessions SET expires_at = $1 WHERE id = $2",
                &[&new_expiry, &session_id],
            )
            .await
            .context("Failed to extend session")?;

        Ok(result > 0)
    }

    /// Revoke a session
    pub async fn revoke(&self, session_id: Uuid) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let result = client
            .execute("DELETE FROM sessions WHERE id = $1", &[&session_id])
            .await
            .context("Failed to revoke session")?;

        Ok(result > 0)
    }

    /// Revoke all sessions for a user
    pub async fn revoke_all_for_user(&self, user_id: Uuid) -> Result<usize> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let result = client
            .execute("DELETE FROM sessions WHERE user_id = $1", &[&user_id])
            .await
            .context("Failed to revoke user sessions")?;

        Ok(result as usize)
    }

    /// Clean up expired sessions
    pub async fn cleanup_expired(&self) -> Result<usize> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let result = client
            .execute("DELETE FROM sessions WHERE expires_at < $1", &[&Utc::now()])
            .await
            .context("Failed to cleanup expired sessions")?;

        debug!("Cleaned up {} expired sessions", result);
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
        user_agent: Option<String>,
        expires_in_hours: i64,
    ) -> anyhow::Result<(services::auth::Session, String)> {
        let (db_session, token) = self
            .create(user_id.0, ip_address, user_agent, expires_in_hours)
            .await?;

        let service_session = services::auth::Session {
            id: services::auth::SessionId(db_session.id.to_string()),
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
        session_token: Uuid,
    ) -> anyhow::Result<Option<services::auth::Session>> {
        let maybe_session = self.validate(session_token).await?;

        Ok(maybe_session.map(|db_session| services::auth::Session {
            id: services::auth::SessionId(db_session.id.to_string()),
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
        user_id: services::auth::UserId,
    ) -> anyhow::Result<Option<services::auth::Session>> {
        let maybe_session = self.get_by_id(user_id.0).await?;

        Ok(maybe_session.map(|db_session| services::auth::Session {
            id: services::auth::SessionId(db_session.id.to_string()),
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
                id: services::auth::SessionId(db_session.id.to_string()),
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
        let session_uuid = Uuid::parse_str(&session_id.0)?;
        self.extend(session_uuid, additional_hours).await
    }

    async fn revoke(&self, session_id: services::auth::SessionId) -> anyhow::Result<bool> {
        let session_uuid = Uuid::parse_str(&session_id.0)?;
        self.revoke(session_uuid).await
    }

    async fn revoke_all_for_user(&self, user_id: services::auth::UserId) -> anyhow::Result<usize> {
        self.revoke_all_for_user(user_id.0).await
    }

    async fn cleanup_expired(&self) -> anyhow::Result<usize> {
        self.cleanup_expired().await
    }
}
