use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use services::admin_settings::{AdminSettingsRepository, StoredSetting};
use services::common::RepositoryError;
use tokio_postgres::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct PostgresAdminSettingsRepository {
    pool: DbPool,
}

impl PostgresAdminSettingsRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn row_to_setting(row: Row) -> StoredSetting {
        StoredSetting {
            key: row.get("key"),
            value: row.get("value"),
            updated_by_user_id: row.get("updated_by_user_id"),
            updated_at: row.get("updated_at"),
        }
    }
}

#[async_trait]
impl AdminSettingsRepository for PostgresAdminSettingsRepository {
    async fn get_all(&self) -> Result<Vec<StoredSetting>> {
        let rows = retry_db!("get_all_admin_settings", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query("SELECT * FROM admin_settings ORDER BY key", &[])
                .await
                .map_err(map_db_error)
        })?;
        Ok(rows.into_iter().map(Self::row_to_setting).collect())
    }

    async fn get(&self, key: &str) -> Result<Option<StoredSetting>> {
        let row = retry_db!("get_admin_setting", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt("SELECT * FROM admin_settings WHERE key = $1", &[&key])
                .await
                .map_err(map_db_error)
        })?;
        Ok(row.map(Self::row_to_setting))
    }

    async fn upsert(&self, key: &str, value: Value, by_user: Uuid) -> Result<StoredSetting> {
        let row = retry_db!("upsert_admin_setting", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    INSERT INTO admin_settings (key, value, updated_by_user_id, updated_at)
                    VALUES ($1, $2, $3, now())
                    ON CONFLICT (key) DO UPDATE SET
                        value = EXCLUDED.value,
                        updated_by_user_id = EXCLUDED.updated_by_user_id,
                        updated_at = now()
                    RETURNING *
                    "#,
                    &[&key, &value, &by_user],
                )
                .await
                .map_err(map_db_error)
        })?;
        Ok(Self::row_to_setting(row))
    }
}
