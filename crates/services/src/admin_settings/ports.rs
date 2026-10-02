use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

/// One stored setting: a JSON value under a key.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSetting {
    pub key: String,
    pub value: Value,
    pub updated_by_user_id: Option<Uuid>,
    pub updated_at: DateTime<Utc>,
}

#[async_trait]
pub trait AdminSettingsRepository: Send + Sync {
    async fn get_all(&self) -> Result<Vec<StoredSetting>>;

    async fn get(&self, key: &str) -> Result<Option<StoredSetting>>;

    /// Replaces the value under `key` (creating it) and records `by_user`.
    async fn upsert(&self, key: &str, value: Value, by_user: Uuid) -> Result<StoredSetting>;
}
