use crate::pool::DbPool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use deadpool_postgres::GenericClient;
use sha2::{Digest, Sha256};
use std::time::Duration;
use uuid::Uuid;

pub struct PgApiKeyModelAffinityRepository {
    pool: DbPool,
}

impl PgApiKeyModelAffinityRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn advisory_lock_key(api_key_id: Uuid, model_name: &str) -> i64 {
        // PostgreSQL advisory locks take a signed 64-bit key, so we hash the logical
        // identifier `(api_key_id, model_name)` down to a deterministic i64. Taking the
        // first 8 bytes of SHA-256 gives us a stable 64-bit value without introducing a
        // custom serialization format, and masking with `i64::MAX` clears the sign bit so
        // the lock key stays non-negative in logs/debugging output.
        let mut hasher = Sha256::new();
        hasher.update(api_key_id.as_bytes());
        hasher.update([0]);
        hasher.update(model_name.as_bytes());

        let digest = hasher.finalize();
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(&digest[..8]);

        i64::from_be_bytes(bytes) & i64::MAX
    }

    async fn get_active_provider_url_with_client<C>(
        client: &C,
        api_key_id: Uuid,
        model_name: &str,
    ) -> Result<Option<String>>
    where
        C: GenericClient + Sync,
    {
        let now = Utc::now();
        let row = client
            .query_opt(
                r#"
                SELECT provider_url
                FROM api_key_model_affinity
                WHERE api_key_id = $1
                  AND model_name = $2
                  AND expires_at > $3
                "#,
                &[&api_key_id, &model_name, &now],
            )
            .await
            .context("Failed to get api key model affinity binding")?;

        Ok(row.map(|row| row.get("provider_url")))
    }

    async fn upsert_provider_url_with_client<C>(
        client: &C,
        api_key_id: Uuid,
        model_name: &str,
        provider_url: &str,
        ttl: Duration,
    ) -> Result<()>
    where
        C: GenericClient + Sync,
    {
        let now = Utc::now();
        let ttl = ChronoDuration::from_std(ttl).context("Invalid affinity TTL")?;
        let expires_at = now + ttl;

        client
            .execute(
                r#"
                INSERT INTO api_key_model_affinity (
                    api_key_id,
                    model_name,
                    provider_url,
                    expires_at,
                    updated_at
                )
                VALUES ($1, $2, $3, $4, $5)
                ON CONFLICT (api_key_id, model_name)
                DO UPDATE SET
                    provider_url = EXCLUDED.provider_url,
                    expires_at = EXCLUDED.expires_at,
                    updated_at = EXCLUDED.updated_at
                "#,
                &[&api_key_id, &model_name, &provider_url, &expires_at, &now],
            )
            .await
            .context("Failed to upsert api key model affinity binding")?;

        Ok(())
    }
}

#[async_trait]
impl services::completions::ports::ApiKeyModelAffinityRepository
    for PgApiKeyModelAffinityRepository
{
    async fn get_or_create_active_provider_url(
        &self,
        api_key_id: Uuid,
        model_name: &str,
        ttl: Duration,
        selector: &(dyn services::completions::ports::ProviderUrlSelector + Send + Sync),
    ) -> Result<Option<String>> {
        let mut client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        if let Some(provider_url) =
            Self::get_active_provider_url_with_client(&client, api_key_id, model_name).await?
        {
            return Ok(Some(provider_url));
        }

        let lock_key = Self::advisory_lock_key(api_key_id, model_name);

        let transaction = client
            .transaction()
            .await
            .context("Failed to start api key model affinity transaction")?;

        transaction
            .execute("SELECT pg_advisory_xact_lock($1)", &[&lock_key])
            .await
            .context("Failed to acquire transaction-scoped api key model affinity advisory lock")?;

        let provider_url = async {
            if let Some(provider_url) =
                Self::get_active_provider_url_with_client(&transaction, api_key_id, model_name)
                    .await?
            {
                return Ok(Some(provider_url));
            }

            let provider_url = selector.select_provider_url().await?;
            if let Some(provider_url) = provider_url.as_deref() {
                Self::upsert_provider_url_with_client(
                    &transaction,
                    api_key_id,
                    model_name,
                    provider_url,
                    ttl,
                )
                .await?;
            }

            Ok(provider_url)
        }
        .await;

        match provider_url {
            Ok(provider_url) => {
                transaction
                    .commit()
                    .await
                    .context("Failed to commit api key model affinity transaction")?;
                Ok(provider_url)
            }
            Err(error) => {
                drop(transaction);
                Err(error)
            }
        }
    }

    async fn get_active_provider_url(
        &self,
        api_key_id: Uuid,
        model_name: &str,
    ) -> Result<Option<String>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        Self::get_active_provider_url_with_client(&client, api_key_id, model_name).await
    }

    async fn upsert_provider_url(
        &self,
        api_key_id: Uuid,
        model_name: &str,
        provider_url: &str,
        ttl: Duration,
    ) -> Result<()> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        Self::upsert_provider_url_with_client(&client, api_key_id, model_name, provider_url, ttl)
            .await
    }
}
