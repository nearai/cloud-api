use crate::pool::DbPool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use std::time::Duration;
use uuid::Uuid;

pub struct PgApiKeyModelAffinityRepository {
    pool: DbPool,
}

impl PgApiKeyModelAffinityRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl services::completions::ports::ApiKeyModelAffinityRepository
    for PgApiKeyModelAffinityRepository
{
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
