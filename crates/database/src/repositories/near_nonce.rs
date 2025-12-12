use crate::pool::DbPool;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use services::auth::NearNonceRepository;

pub struct PostgresNearNonceRepository {
    pool: DbPool,
}

impl PostgresNearNonceRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl NearNonceRepository for PostgresNearNonceRepository {
    async fn consume_nonce(&self, nonce_hex: &str) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let result = client
            .execute(
                "INSERT INTO near_used_nonces (nonce_hex) VALUES ($1) ON CONFLICT DO NOTHING",
                &[&nonce_hex],
            )
            .await?;
        Ok(result > 0)
    }

    async fn cleanup_expired_nonces(&self) -> anyhow::Result<u64> {
        let client = self.pool.get().await?;
        let cutoff = Utc::now() - Duration::minutes(10);
        let deleted = client
            .execute(
                "DELETE FROM near_used_nonces WHERE used_at < $1",
                &[&cutoff],
            )
            .await?;
        Ok(deleted)
    }
}
