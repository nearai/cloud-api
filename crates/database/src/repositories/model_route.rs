use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use services::common::RepositoryError;
use services::inference_provider_pool::{ModelRoute, ModelRoutesSource};
use tokio_postgres::error::SqlState;
use tokio_postgres::GenericClient;

/// At most one "routes table unavailable" warning per this many seconds.
const MISSING_TABLE_WARNING_INTERVAL_SECS: u64 = 600;
/// Unix seconds of the last such warning.
static LAST_MISSING_TABLE_WARNING: AtomicU64 = AtomicU64::new(0);

fn warn_routes_table_missing() {
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
            "Model routes table is unavailable; no model has routes"
        );
    }
}

/// The enabled routes between active third-party catalog entries, read on
/// `client`; `None` when its schema has no routes table (SQLSTATE 42P01),
/// where no route can exist.
pub async fn query_model_routes<C: GenericClient + Sync>(
    client: &C,
) -> Result<Option<Vec<ModelRoute>>, RepositoryError> {
    let rows = match client
        .query(
            "SELECT r.id, m.model_name, t.model_name AS target_model_name, r.priority
             FROM model_routes r
             JOIN models m ON m.id = r.model_id
             JOIN models t ON t.id = r.target_model_id
             WHERE r.is_enabled
               AND m.is_active AND t.is_active
               AND m.provider_type = 'external' AND NOT m.attestation_supported
               AND t.provider_type = 'external' AND NOT t.attestation_supported
             ORDER BY m.model_name, r.priority, t.model_name",
            &[],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) if err.code() == Some(&SqlState::UNDEFINED_TABLE) => return Ok(None),
        Err(err) => return Err(map_db_error(err)),
    };
    rows.iter()
        .map(|row| {
            Ok(ModelRoute {
                id: row.try_get("id")?,
                model: row.try_get("model_name")?,
                target: row.try_get("target_model_name")?,
                priority: row.try_get("priority")?,
            })
        })
        .collect::<Result<Vec<_>, tokio_postgres::Error>>()
        .context("Failed to read a model route")
        .map_err(RepositoryError::DataConversionError)
        .map(Some)
}

/// Reads the model routes (`model_routes`). Never writes them.
#[derive(Debug, Clone)]
pub struct PgModelRouteRepository {
    pool: DbPool,
}

impl PgModelRouteRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn list_enabled(&self) -> Result<Vec<ModelRoute>> {
        let routes = retry_db!("list_enabled_model_routes", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            query_model_routes(&**client).await
        })?;

        // No routes table: no model has routes.
        Ok(routes.unwrap_or_else(|| {
            warn_routes_table_missing();
            Vec::new()
        }))
    }
}

#[async_trait]
impl ModelRoutesSource for PgModelRouteRepository {
    async fn fetch_model_routes(&self) -> Result<Vec<ModelRoute>, String> {
        self.list_enabled()
            .await
            .map_err(|e| format!("Failed to fetch model routes: {e}"))
    }
}
