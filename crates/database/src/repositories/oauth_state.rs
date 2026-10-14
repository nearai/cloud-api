use crate::pool::DbPool;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use tracing::debug;

pub struct OAuthStateRepository {
    pool: DbPool,
}

#[derive(Debug, Clone)]
pub struct OAuthStateRow {
    pub state: String,
    pub provider: String,
    pub pkce_verifier: Option<String>,
    pub frontend_callback: Option<String>,
    pub link_user_id: Option<uuid::Uuid>,
    pub link_session_id: Option<uuid::Uuid>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl OAuthStateRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Create new OAuth state with 10-minute expiration
    pub async fn create(
        &self,
        state: String,
        provider: String,
        pkce_verifier: Option<String>,
        frontend_callback: Option<String>,
    ) -> Result<OAuthStateRow> {
        self.create_context(state, provider, pkce_verifier, frontend_callback, None)
            .await
    }

    pub async fn create_link(
        &self,
        state: String,
        provider: String,
        verifier: String,
        callback: String,
        user_id: uuid::Uuid,
        session_id: uuid::Uuid,
    ) -> Result<OAuthStateRow> {
        self.create_context(
            state,
            provider,
            Some(verifier),
            Some(callback),
            Some((user_id, session_id)),
        )
        .await
    }

    async fn create_context(
        &self,
        state: String,
        provider: String,
        pkce_verifier: Option<String>,
        frontend_callback: Option<String>,
        link: Option<(uuid::Uuid, uuid::Uuid)>,
    ) -> Result<OAuthStateRow> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();
        let expires_at = now + Duration::minutes(10);
        let user_id = link.map(|(user, _)| user);
        let session_id = link.map(|(_, session)| session);

        let row = client
            .query_one(
                r#"
                INSERT INTO oauth_states (state, provider, pkce_verifier, frontend_callback, created_at, expires_at,link_user_id,link_session_id)
                VALUES ($1, $2, $3, $4, $5, $6,$7,$8)
                RETURNING state, provider, pkce_verifier, frontend_callback, created_at, expires_at,link_user_id,link_session_id
                "#,
                &[&state, &provider, &pkce_verifier, &frontend_callback, &now, &expires_at, &user_id, &session_id],
            )
            .await
            .context("Failed to create OAuth state")?;

        debug!(
            "Created OAuth state for provider: {} (expires in 10 minutes)",
            provider
        );

        Ok(self.row_to_oauth_state(&row))
    }

    /// Get and atomically delete state (prevents replay attacks)
    /// Returns None if state doesn't exist or has expired
    pub async fn get_and_delete(&self, state: &str) -> Result<Option<OAuthStateRow>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();

        let row = client
            .query_opt(
                r#"
                DELETE FROM oauth_states
                WHERE state = $1 AND expires_at > $2
                RETURNING state, provider, pkce_verifier, frontend_callback, created_at, expires_at,link_user_id,link_session_id
                "#,
                &[&state, &now],
            )
            .await
            .context("Failed to get and delete OAuth state")?;

        match row {
            Some(r) => {
                let oauth_state = self.row_to_oauth_state(&r);
                debug!(
                    "Retrieved and deleted OAuth state for provider: {}",
                    oauth_state.provider
                );
                Ok(Some(oauth_state))
            }
            None => {
                let state_present = !state.is_empty();
                let state_len = state.len();
                debug!(state_present, state_len, "OAuth state not found or expired");
                Ok(None)
            }
        }
    }

    /// Clean up expired OAuth states
    pub async fn cleanup_expired(&self) -> Result<usize> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();
        let result = client
            .execute("DELETE FROM oauth_states WHERE expires_at <= $1", &[&now])
            .await
            .context("Failed to cleanup expired OAuth states")?;

        debug!("Cleaned up {} expired OAuth states", result);
        Ok(result as usize)
    }

    /// Helper method to convert database row to OAuthStateRow
    fn row_to_oauth_state(&self, row: &tokio_postgres::Row) -> OAuthStateRow {
        OAuthStateRow {
            state: row.get("state"),
            provider: row.get("provider"),
            pkce_verifier: row.get("pkce_verifier"),
            frontend_callback: row.get("frontend_callback"),
            link_user_id: row.get("link_user_id"),
            link_session_id: row.get("link_session_id"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
        }
    }
}
