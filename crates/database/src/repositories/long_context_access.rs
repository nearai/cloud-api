use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use services::common::RepositoryError;
use services::long_context_access::LongContextAccessRepository;
use tokio_postgres::error::SqlState;
use uuid::Uuid;

/// At most one "blocks table unavailable" warning per this many seconds.
const MISSING_TABLE_WARNING_INTERVAL_SECS: u64 = 600;
/// Unix seconds of the last such warning.
static LAST_MISSING_TABLE_WARNING: AtomicU64 = AtomicU64::new(0);

fn warn_blocks_table_missing() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let last = LAST_MISSING_TABLE_WARNING.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= MISSING_TABLE_WARNING_INTERVAL_SECS
        && LAST_MISSING_TABLE_WARNING
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!(
            error_kind = "undefined_table",
            "Long-context blocks table is unavailable; no organization is blocked"
        );
    }
}

/// Reads what the long-context access check needs: the per-model blocks
/// (`organization_model_long_context_blocks`) and an organization's
/// settings. Never writes either.
#[derive(Debug, Clone)]
pub struct PgLongContextAccessRepository {
    pool: DbPool,
}

impl PgLongContextAccessRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl LongContextAccessRepository for PgLongContextAccessRepository {
    async fn blocked_model_names(&self, organization_id: Uuid) -> Result<Vec<String>> {
        // `None`: the schema has no blocks table, so nothing is blocked.
        let rows = retry_db!("list_organization_long_context_blocks", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            match client
                .query(
                    "SELECT m.model_name
                     FROM organization_model_long_context_blocks b
                     JOIN models m ON m.id = b.model_id
                     WHERE b.organization_id = $1",
                    &[&organization_id],
                )
                .await
            {
                Ok(rows) => Ok(Some(rows)),
                Err(err) if err.code() == Some(&SqlState::UNDEFINED_TABLE) => Ok(None),
                Err(err) => Err(map_db_error(err)),
            }
        })?;

        let Some(rows) = rows else {
            warn_blocks_table_missing();
            return Ok(Vec::new());
        };
        rows.iter()
            .map(|row| {
                row.try_get::<_, String>("model_name")
                    .context("Failed to read a blocked model name")
            })
            .collect()
    }

    async fn organization_settings(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<serde_json::Value>> {
        let row = retry_db!("get_organization_long_context_settings", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT settings FROM organizations WHERE id = $1",
                    &[&organization_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(row) => row
                .try_get::<_, Option<serde_json::Value>>("settings")
                .context("Failed to read organization settings"),
            None => Ok(None),
        }
    }
}
