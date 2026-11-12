//! Analytics repository implementation for enterprise dashboard queries.
//!
//! All costs use fixed scale 9 (nano-dollars) and USD currency. This impl owns client
//! acquisition, transactions and timeouts; report SQL lives in one file per report family.
//! Every report runs in one read-only transaction under one statement budget (spec §6.3).

mod organization;

use super::{reporting_query, utils::map_db_error};
use crate::pool::DbPool;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use services::admin::{AnalyticsRepository, OrganizationMetrics, TimeSeriesMetrics};
use services::common::RepositoryError;
use std::time::{Duration, Instant};
use tokio_postgres::Transaction;
use uuid::Uuid;

/// Budget for one analytics report (spec §6.3), shared by all of its statements.
const ANALYTICS_STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);

/// PostgreSQL implementation of the analytics repository
pub struct PgAnalyticsRepository {
    pool: DbPool,
    statement_timeout: Duration,
}

impl PgAnalyticsRepository {
    pub fn new(pool: DbPool) -> Self {
        Self::with_statement_timeout(pool, ANALYTICS_STATEMENT_TIMEOUT)
    }

    /// Injects the budget for every report except revenue density. Tests pass a short one
    /// to force `RepositoryError::QueryTimeout`.
    pub fn with_statement_timeout(pool: DbPool, statement_timeout: Duration) -> Self {
        Self {
            pool,
            statement_timeout,
        }
    }

    /// Runs one report in its own read-only transaction under a deadline of `budget` from
    /// now. The deadline starts before the pool wait, so the wait counts against it. The
    /// report arms the deadline (`arm`) before each of its statements.
    async fn run_report<T>(
        &self,
        budget: Duration,
        report: impl AsyncFnOnce(&Transaction<'_>, Instant) -> Result<T, RepositoryError>,
    ) -> Result<T, RepositoryError> {
        let deadline = reporting_query::reporting_deadline(budget, None)?;
        let mut client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;
        let transaction = client
            .build_transaction()
            .read_only(true)
            .start()
            .await
            .map_err(map_db_error)?;
        let result = report(&transaction, deadline).await?;
        transaction.commit().await.map_err(map_db_error)?;
        Ok(result)
    }
}

/// Convert nano-dollars (scale 9) to USD
fn nano_to_usd(nano: i64) -> f64 {
    nano as f64 / 1_000_000_000.0
}

/// Re-arms the transaction-local `statement_timeout` with what is left of the report's
/// deadline, so one budget covers every statement of a report, and pins UTC. `SET LOCAL`
/// semantics: nothing leaks onto the pooled connection.
async fn arm(tx: &Transaction<'_>, deadline: Instant) -> Result<(), RepositoryError> {
    reporting_query::configure_reporting_transaction(
        tx,
        reporting_query::remaining_statement_timeout(deadline)?,
    )
    .await
}

/// Count-weighted combination of per-hour percentiles (spec §4): exact within one
/// `usage_hourly` row, approximate across rows (documented as approx. in the API). NULL
/// when no row has samples, as `PERCENTILE_CONT` over no values is.
fn approx_percentile(percentile: &str, samples: &str) -> String {
    format!("(SUM({percentile} * {samples}) / NULLIF(SUM({samples}), 0)::double precision)")
}

/// Exact mean from per-hour sums and sample counts; NULL when there are no samples.
fn weighted_mean(sum: &str, samples: &str) -> String {
    format!("(SUM({sum})::double precision / NULLIF(SUM({samples}), 0)::double precision)")
}

#[async_trait]
impl AnalyticsRepository for PgAnalyticsRepository {
    async fn get_organization_metrics(
        &self,
        org_id: Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        credit_type: Option<&str>,
    ) -> Result<OrganizationMetrics, RepositoryError> {
        self.run_report(self.statement_timeout, async |tx, deadline| {
            organization::get_organization_metrics_with_client(
                tx,
                deadline,
                (org_id, start, end),
                credit_type,
            )
            .await
        })
        .await
    }

    async fn get_organization_timeseries(
        &self,
        org_id: Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        granularity: &str,
        credit_type: Option<&str>,
    ) -> Result<TimeSeriesMetrics, RepositoryError> {
        self.run_report(self.statement_timeout, async |tx, deadline| {
            organization::get_organization_timeseries_with_client(
                tx,
                deadline,
                (org_id, start, end),
                granularity,
                credit_type,
            )
            .await
        })
        .await
    }
}
