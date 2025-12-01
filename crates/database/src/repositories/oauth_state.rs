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
                INSERT INTO oauth_states (state, provider, pkce_verifier, created_at, expires_at)
                VALUES ($1, $2, $3, $4, $5)
                RETURNING state, provider, pkce_verifier, created_at, expires_at
                "#,
                &[&state, &provider, &pkce_verifier, &now, &expires_at],
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
                RETURNING state, provider, pkce_verifier, created_at, expires_at
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

    /// Helper method to convert database row to OAuthStateRow
    fn row_to_oauth_state(&self, row: &tokio_postgres::Row) -> OAuthStateRow {
        OAuthStateRow {
            state: row.get("state"),
            provider: row.get("provider"),
            pkce_verifier: row.get("pkce_verifier"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_test_pool() -> DbPool {
        let config = config::DatabaseConfig {
            primary_app_id: "postgres-test".to_string(),
            port: 5432,
            host: None,
            database: "platform_api".to_string(),
            username: std::env::var("DATABASE_USERNAME").unwrap_or("postgres".to_string()),
            password: std::env::var("DATABASE_PASSWORD").unwrap_or("postgres".to_string()),
            max_connections: 2,
            tls_enabled: false,
            tls_ca_cert_path: None,
            refresh_interval: 30,
            mock: false,
        };

        crate::Database::from_config(&config)
            .await
            .unwrap()
            .pool()
            .clone()
    }

    #[tokio::test]
    async fn test_create_and_get_oauth_state() {
        let pool = create_test_pool().await;
        let repo = OAuthStateRepository::new(pool.clone());

        let state = format!("test-state-{}", uuid::Uuid::new_v4());
        let provider = "github".to_string();

        // Create state
        let created = repo
            .create(state.clone(), provider.clone(), None)
            .await
            .unwrap();
        assert_eq!(created.state, state);
        assert_eq!(created.provider, provider);
        assert_eq!(created.pkce_verifier, None);

        // Get and delete state
        let retrieved = repo.get_and_delete(&state).await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.state, state);
        assert_eq!(retrieved.provider, provider);

        // Second get should return None (state was deleted)
        let second_get = repo.get_and_delete(&state).await.unwrap();
        assert!(second_get.is_none());
    }

    #[tokio::test]
    async fn test_expired_state_not_returned() {
        let pool = create_test_pool().await;
        let repo = OAuthStateRepository::new(pool.clone());

        let state = format!("test-state-{}", uuid::Uuid::new_v4());

        // Create state with past expiration
        let client = pool.get().await.unwrap();
        let past_time = Utc::now() - Duration::minutes(1);
        client
            .execute(
                r#"
                INSERT INTO oauth_states (state, provider, pkce_verifier, created_at, expires_at)
                VALUES ($1, $2, $3, $4, $5)
                "#,
                &[&state, &"github", &None::<String>, &past_time, &past_time],
            )
            .await
            .unwrap();

        // Try to get expired state
        let result = repo.get_and_delete(&state).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_google_with_pkce_verifier() {
        let pool = create_test_pool().await;
        let repo = OAuthStateRepository::new(pool.clone());

        let state = format!("test-state-{}", uuid::Uuid::new_v4());
        let provider = "google".to_string();
        let verifier = Some("test-pkce-verifier".to_string());

        // Create state with PKCE verifier
        let created = repo
            .create(state.clone(), provider.clone(), verifier.clone())
            .await
            .unwrap();
        assert_eq!(created.pkce_verifier, verifier);

        // Get and verify PKCE verifier is preserved
        let retrieved = repo.get_and_delete(&state).await.unwrap().unwrap();
        assert_eq!(retrieved.pkce_verifier, verifier);
    }

    #[tokio::test]
    async fn test_state_replay_protection() {
        let pool = create_test_pool().await;
        let repo = OAuthStateRepository::new(pool.clone());

        let state = format!("test-state-{}", uuid::Uuid::new_v4());
        let provider = "github".to_string();

        // Create one state
        repo.create(state.clone(), provider, None).await.unwrap();

        // First get should succeed
        let first = repo.get_and_delete(&state).await.unwrap();
        assert!(first.is_some());

        // Second get should fail (replay protection)
        let second = repo.get_and_delete(&state).await.unwrap();
        assert!(second.is_none());
    }
}
