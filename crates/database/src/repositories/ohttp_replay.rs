use crate::DbPool;

/// Durable admission of an OHTTP encapsulation. Claims must be retained while
/// the corresponding key configuration can still decrypt requests.
pub struct OhttpReplayRepository {
    pool: DbPool,
}

impl OhttpReplayRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn claim(&self, config_hash: &[u8], request_hash: &[u8]) -> anyhow::Result<bool> {
        let connection = self.pool.get().await?;
        let inserted = connection.execute(
            "INSERT INTO ohttp_request_claims (key_config_hash, encapsulation_hash) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[&config_hash, &request_hash],
        ).await?;
        Ok(inserted == 1)
    }
}
