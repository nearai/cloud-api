use crate::models::ApiKey;
use crate::pool::DbPool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use services::auth::ports::{CreateApiKeyRequest, WorkspaceId};
use sha2::{Digest, Sha256};
use tracing::debug;
use uuid::Uuid;

pub struct ApiKeyRepository {
    pool: DbPool,
}

impl ApiKeyRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Generate a new API key
    pub fn generate_api_key() -> String {
        format!("sk_{}", Uuid::new_v4().to_string().replace("-", ""))
    }

    /// Hash an API key for storage
    fn hash_api_key(key: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Extract key prefix from a generated key for display purposes
    fn extract_key_prefix(key: &str) -> String {
        // Take first 10 characters (e.g., "sk_abc1234" from "sk_abc1234567890...")
        let prefix_len = 10.min(key.len());
        key[..prefix_len].to_string()
    }

    /// Create a new API key
    pub async fn create(&self, request: CreateApiKeyRequest) -> Result<(String, ApiKey)> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let id = Uuid::new_v4();
        let key = Self::generate_api_key();
        let key_hash = Self::hash_api_key(&key);
        let key_prefix = Self::extract_key_prefix(&key);
        let now = Utc::now();

        // Use name as Option<String>
        let name = request.name.clone().unwrap_or_default();

        let _row = client
            .query_one(
                r#"
                INSERT INTO api_keys (
                    id, key_hash, key_prefix, name, workspace_id, created_by_user_id,
                    created_at, expires_at, last_used_at, is_active
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL, true)
                RETURNING *
                "#,
                &[
                    &id,
                    &key_hash,
                    &key_prefix,
                    &name,
                    &request.workspace_id.0,
                    &request.created_by_user_id.0,
                    &now,
                    &request.expires_at,
                ],
            )
            .await
            .context("Failed to create API key")?;

        debug!(
            "Created API key: {} for workspace: {} by user: {}",
            id, request.workspace_id.0, request.created_by_user_id.0
        );

        Ok((
            key,
            ApiKey {
                id,
                key_hash,
                key_prefix,
                name,
                created_at: now,
                expires_at: request.expires_at,
                last_used_at: None,
                is_active: true,
                created_by_user_id: request.created_by_user_id.0,
                workspace_id: request.workspace_id.0,
                spend_limit: None,
            },
        ))
    }

    /// Create a new API key and return it with the raw key for API response
    pub async fn create_with_key(
        &self,
        request: CreateApiKeyRequest,
    ) -> Result<crate::models::ApiKeyResponse> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let id = Uuid::new_v4();
        let key = Self::generate_api_key();
        let key_hash = Self::hash_api_key(&key);
        let key_prefix = Self::extract_key_prefix(&key);
        let now = Utc::now();

        // Use name as Option<String>
        let name = request.name.clone().unwrap_or_default();

        let _row = client
            .query_one(
                r#"
                INSERT INTO api_keys (
                    id, key_hash, key_prefix, name, workspace_id, created_by_user_id,
                    created_at, expires_at, last_used_at, is_active
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL, true)
                RETURNING *
                "#,
                &[
                    &id,
                    &key_hash,
                    &key_prefix,
                    &name,
                    &request.workspace_id.0,
                    &request.created_by_user_id.0,
                    &now,
                    &request.expires_at,
                ],
            )
            .await
            .context("Failed to create API key")?;

        debug!(
            "Created API key: {} for workspace: {} by user: {}",
            id, request.workspace_id.0, request.created_by_user_id.0
        );

        Ok(crate::models::ApiKeyResponse {
            id,
            key, // Return the raw key for the API response
            name,
            created_at: now,
            expires_at: request.expires_at,
        })
    }

    /// Get an API key by ID
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<ApiKey>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                "SELECT * FROM api_keys WHERE id = $1 AND is_active = true",
                &[&id],
            )
            .await
            .context("Failed to query API key")?;

        match row {
            Some(row) => Ok(Some(self.row_to_api_key(row)?)),
            None => Ok(None),
        }
    }

    /// Get an API key by its hash
    pub async fn get_by_hash(&self, key_hash: &str) -> Result<Option<ApiKey>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                "SELECT * FROM api_keys WHERE key_hash = $1 AND is_active = true",
                &[&key_hash],
            )
            .await
            .context("Failed to query API key by hash")?;

        match row {
            Some(row) => Ok(Some(self.row_to_api_key(row)?)),
            None => Ok(None),
        }
    }

    /// Validate an API key globally and return it if valid
    /// API keys are globally unique across all workspaces
    pub async fn validate(&self, key: &str) -> Result<Option<ApiKey>> {
        let key_hash = Self::hash_api_key(key);

        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                r#"
            SELECT * FROM api_keys 
            WHERE key_hash = $1 
              AND is_active = true 
              AND (expires_at IS NULL OR expires_at > NOW())
            "#,
                &[&key_hash],
            )
            .await
            .context("Failed to validate API key")?;

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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        client
            .execute(
                "UPDATE api_keys SET last_used_at = NOW() WHERE id = $1",
                &[&id],
            )
            .await
            .context("Failed to update last used timestamp")?;

        Ok(())
    }

    /// List API keys for a workspace
    pub async fn list_by_workspace(&self, workspace_id: Uuid) -> Result<Vec<ApiKey>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client.query(
            "SELECT * FROM api_keys WHERE workspace_id = $1 AND is_active = true ORDER BY created_at DESC",
            &[&workspace_id],
        ).await.context("Failed to list API keys")?;

        rows.into_iter()
            .map(|row| self.row_to_api_key(row))
            .collect()
    }

    /// List API keys created by a user
    pub async fn list_by_user(&self, user_id: Uuid) -> Result<Vec<ApiKey>> {
        let client = self
            .pool
            .get()
            .await
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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows_affected = client
            .execute(
                "UPDATE api_keys SET is_active = false WHERE id = $1",
                &[&id],
            )
            .await
            .context("Failed to revoke API key")?;

        Ok(rows_affected > 0)
    }

    /// Delete expired API keys
    pub async fn cleanup_expired(&self) -> Result<u64> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows_affected = client.execute(
            "UPDATE api_keys SET is_active = false WHERE expires_at < NOW() AND is_active = true",
            &[],
        ).await.context("Failed to cleanup expired API keys")?;

        Ok(rows_affected)
    }

    /// Get workspace info for an API key - used for auth resolution
    pub async fn get_workspace_for_api_key(
        &self,
        api_key: &ApiKey,
    ) -> Result<Option<crate::models::Workspace>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                "SELECT * FROM workspaces WHERE id = $1 AND is_active = true",
                &[&api_key.workspace_id],
            )
            .await
            .context("Failed to query workspace for API key")?;

        match row {
            Some(row) => Ok(Some(crate::models::Workspace {
                id: row.get("id"),
                name: row.get("name"),
                display_name: row.get("display_name"),
                description: row.get("description"),
                organization_id: row.get("organization_id"),
                created_by_user_id: row.get("created_by_user_id"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
                is_active: row.get("is_active"),
                settings: row.get("settings"),
            })),
            None => Ok(None),
        }
    }

    /// Update spend limit for an API key
    pub async fn update_spend_limit(&self, id: Uuid, spend_limit: Option<i64>) -> Result<ApiKey> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_one(
                "UPDATE api_keys SET spend_limit = $1 WHERE id = $2 AND is_active = true RETURNING *",
                &[&spend_limit, &id],
            )
            .await
            .context("Failed to update API key spend limit")?;

        debug!("Updated spend limit for API key: {}", id);
        self.row_to_api_key(row)
    }

    // Helper function to convert database row to ApiKey
    fn row_to_api_key(&self, row: tokio_postgres::Row) -> Result<ApiKey> {
        Ok(ApiKey {
            id: row.get("id"),
            key_hash: row.get("key_hash"),
            key_prefix: row.get("key_prefix"),
            name: row.get("name"),
            workspace_id: row.get("workspace_id"),
            created_by_user_id: row.get("created_by_user_id"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
            last_used_at: row.get("last_used_at"),
            is_active: row.get("is_active"),
            spend_limit: row.get("spend_limit"),
        })
    }
}

// Convert database ApiKey to service ApiKey
fn db_apikey_to_service_apikey(
    api_key: Option<String>,
    db_api_key: ApiKey,
) -> services::auth::ApiKey {
    services::auth::ApiKey {
        id: services::auth::ports::ApiKeyId(db_api_key.id.to_string()),
        key: api_key,
        key_prefix: db_api_key.key_prefix,
        name: db_api_key.name,
        workspace_id: WorkspaceId(db_api_key.workspace_id),
        created_by_user_id: services::auth::ports::UserId(db_api_key.created_by_user_id),
        created_at: db_api_key.created_at,
        expires_at: db_api_key.expires_at,
        last_used_at: db_api_key.last_used_at,
        is_active: db_api_key.is_active,
        spend_limit: db_api_key.spend_limit,
    }
}

// Implement the service trait
#[async_trait]
impl services::auth::ports::ApiKeyRepository for ApiKeyRepository {
    async fn validate(&self, api_key: String) -> anyhow::Result<Option<services::auth::ApiKey>> {
        let maybe_api_key = self.validate(&api_key).await?;
        Ok(maybe_api_key.map(|db_api_key| db_apikey_to_service_apikey(None, db_api_key)))
    }

    async fn create(&self, request: CreateApiKeyRequest) -> anyhow::Result<services::auth::ApiKey> {
        let (key, db_api_key) = self.create(request).await?;
        Ok(db_apikey_to_service_apikey(Some(key), db_api_key))
    }

    async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> anyhow::Result<Vec<services::auth::ApiKey>> {
        let api_keys = self.list_by_workspace(workspace_id.0).await?;
        Ok(api_keys
            .into_iter()
            .map(|db_api_key| db_apikey_to_service_apikey(None, db_api_key))
            .collect())
    }

    async fn delete(&self, id: services::auth::ports::ApiKeyId) -> anyhow::Result<bool> {
        self.revoke(Uuid::parse_str(&id.0)?).await
    }

    async fn update_last_used(&self, id: services::auth::ports::ApiKeyId) -> anyhow::Result<()> {
        self.update_last_used(Uuid::parse_str(&id.0)?).await
    }

    async fn update_spend_limit(
        &self,
        id: services::auth::ports::ApiKeyId,
        spend_limit: Option<i64>,
    ) -> anyhow::Result<services::auth::ApiKey> {
        let db_api_key = self
            .update_spend_limit(Uuid::parse_str(&id.0)?, spend_limit)
            .await?;
        Ok(db_apikey_to_service_apikey(None, db_api_key))
    }
}
