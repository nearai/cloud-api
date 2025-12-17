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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();
        let expires_at = now + Duration::minutes(10);

        let row = client
            .query_one(
                r#"
                INSERT INTO oauth_states (state, provider, pkce_verifier, frontend_callback, created_at, expires_at)
                VALUES ($1, $2, $3, $4, $5, $6)
                RETURNING state, provider, pkce_verifier, frontend_callback, created_at, expires_at
                "#,
                &[&state, &provider, &pkce_verifier, &frontend_callback, &now, &expires_at],
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
                RETURNING state, provider, pkce_verifier, frontend_callback, created_at, expires_at
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
                debug!("OAuth state not found or expired: {}", state);
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
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
        }
    }
}
