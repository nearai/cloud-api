use crate::models::{ApiKey, CreateApiKeyRequest, ApiKeyResponse};
use crate::pool::DbPool;
use anyhow::{Result, Context};
use uuid::Uuid;
use chrono::{Utc, Duration};
use sha2::{Sha256, Digest};
use tracing::debug;

pub struct ApiKeyRepository {
    pool: DbPool,
}

impl ApiKeyRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Generate a new API key
    fn generate_api_key() -> String {
        format!("sk_{}", Uuid::new_v4().to_string().replace("-", ""))
    }

    /// Hash an API key for storage
    fn hash_api_key(key: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Create a new API key
    pub async fn create(
        &self, 
        org_id: Uuid,
        user_id: Uuid,
        request: CreateApiKeyRequest
    ) -> Result<ApiKeyResponse> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let id = Uuid::new_v4();
        let key = Self::generate_api_key();
        let key_hash = Self::hash_api_key(&key);
        let now = Utc::now();
        
        let expires_at = request.expires_in_days.map(|days| {
            now + Duration::days(days as i64)
        });
        
        let _row = client.query_one(
            r#"
            INSERT INTO api_keys (
                id, key_hash, name, organization_id, created_by_user_id,
                created_at, expires_at, is_active
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, true)
            RETURNING *
            "#,
            &[
                &id,
                &key_hash,
                &request.name,
                &org_id,
                &user_id,
                &now,
                &expires_at,
            ],
        ).await.context("Failed to create API key")?;
        
        debug!("Created API key: {} for org: {} by user: {}", id, org_id, user_id);
        
        Ok(ApiKeyResponse {
            id,
            key: key.clone(), // Only return the unhashed key on creation
            name: request.name.clone(),
            created_at: now,
            expires_at,
        })
    }

    /// Get an API key by ID
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<ApiKey>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_opt(
            "SELECT * FROM api_keys WHERE id = $1 AND is_active = true",
            &[&id],
        ).await.context("Failed to query API key")?;
        
        match row {
            Some(row) => Ok(Some(self.row_to_api_key(row)?)),
            None => Ok(None),
        }
    }

    /// Get an API key by its hash
    pub async fn get_by_hash(&self, key_hash: &str) -> Result<Option<ApiKey>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_opt(
            "SELECT * FROM api_keys WHERE key_hash = $1 AND is_active = true",
            &[&key_hash],
        ).await.context("Failed to query API key by hash")?;
        
        match row {
            Some(row) => Ok(Some(self.row_to_api_key(row)?)),
            None => Ok(None),
        }
    }

    /// Validate an API key and return it if valid
    pub async fn validate(&self, key: &str) -> Result<Option<ApiKey>> {
        let key_hash = Self::hash_api_key(key);
        
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_opt(
            r#"
            SELECT * FROM api_keys 
            WHERE key_hash = $1 
              AND is_active = true 
              AND (expires_at IS NULL OR expires_at > NOW())
            "#,
            &[&key_hash],
        ).await.context("Failed to validate API key")?;
        
        match row {
            Some(row) => {
                let api_key = self.row_to_api_key(row)?;
                // Update last used timestamp
                let _ = self.update_last_used(api_key.id).await;
                Ok(Some(api_key))
            }
            None => Ok(None),
        }
    }

    /// Update the last used timestamp for an API key
    async fn update_last_used(&self, id: Uuid) -> Result<()> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        client.execute(
            "UPDATE api_keys SET last_used_at = NOW() WHERE id = $1",
            &[&id],
        ).await.context("Failed to update last used timestamp")?;
        
        Ok(())
    }

    /// List API keys for an organization
    pub async fn list_by_organization(&self, org_id: Uuid) -> Result<Vec<ApiKey>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let rows = client.query(
            "SELECT * FROM api_keys WHERE organization_id = $1 AND is_active = true ORDER BY created_at DESC",
            &[&org_id],
        ).await.context("Failed to list API keys")?;
        
        rows.into_iter()
            .map(|row| self.row_to_api_key(row))
            .collect()
    }

    /// List API keys created by a user
    pub async fn list_by_user(&self, user_id: Uuid) -> Result<Vec<ApiKey>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let rows = client.query(
            "SELECT * FROM api_keys WHERE created_by_user_id = $1 AND is_active = true ORDER BY created_at DESC",
            &[&user_id],
        ).await.context("Failed to list user's API keys")?;
        
        rows.into_iter()
            .map(|row| self.row_to_api_key(row))
            .collect()
    }

    /// Revoke an API key
    pub async fn revoke(&self, id: Uuid) -> Result<bool> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let rows_affected = client.execute(
            "UPDATE api_keys SET is_active = false WHERE id = $1",
            &[&id],
        ).await.context("Failed to revoke API key")?;
        
        Ok(rows_affected > 0)
    }

    /// Delete expired API keys
    pub async fn cleanup_expired(&self) -> Result<u64> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let rows_affected = client.execute(
            "UPDATE api_keys SET is_active = false WHERE expires_at < NOW() AND is_active = true",
            &[],
        ).await.context("Failed to cleanup expired API keys")?;
        
        Ok(rows_affected)
    }

    // Helper function to convert database row to ApiKey
    fn row_to_api_key(&self, row: tokio_postgres::Row) -> Result<ApiKey> {
        Ok(ApiKey {
            id: row.get("id"),
            key_hash: row.get("key_hash"),
            name: row.get("name"),
            organization_id: row.get("organization_id"),
            created_by_user_id: row.get("created_by_user_id"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
            last_used_at: row.get("last_used_at"),
            is_active: row.get("is_active"),
        })
    }
}