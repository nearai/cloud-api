//! Analytics repository implementation for enterprise dashboard queries.
//!
//! All costs use fixed scale 9 (nano-dollars) and USD currency. This impl owns client
//! acquisition, transactions and timeouts; report SQL lives in one file per report family.
//! Every report runs in one read-only transaction under one statement budget (spec §6.3).

mod consumption;
mod organization;
mod pagination;
mod platform;
mod provider_attribution;
mod revenue;

use super::{reporting_query, utils::map_db_error};
use crate::pool::DbPool;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use services::admin::{
    AnalyticsRepository, BillingSummary, ModelConsumptionTimeseries,
    ModelConsumptionTimeseriesQuery, ModelRevenueQuery, ModelRevenueReport, OrgRevenueQuery,
    OrgRevenueReport, OrganizationMetrics, PerformanceTimeseries, PerformanceTimeseriesQuery,
    PlatformMetrics, PlatformTimeSeriesMetrics, RevenueDensityQuery, RevenueDensityReport,
    TimeSeriesMetrics,
};
use services::common::RepositoryError;
use std::time::{Duration, Instant};
use tokio_postgres::Transaction;
use uuid::Uuid;

/// Budget for one analytics report (spec §6.3), shared by all of its statements.
const ANALYTICS_STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Revenue density stays on raw per-minute buckets over up to 90 days (spec §8). This is a
/// runaway guard, not a latency target, so it is a fixed constant rather than the seam.
const REVENUE_DENSITY_STATEMENT_TIMEOUT: Duration = Duration::from_secs(120);

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

    fn deadline(&self) -> Result<Instant, RepositoryError> {
        reporting_query::reporting_deadline(self.statement_timeout, None)
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

#[async_trait]
impl AnalyticsRepository for PgAnalyticsRepository {
    async fn get_organization_metrics(
        &self,
        org_id: Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        credit_type: Option<&str>,
    ) -> Result<OrganizationMetrics, RepositoryError> {
        let deadline = self.deadline()?;
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
        let report = organization::get_organization_metrics_with_client(
            &transaction,
            deadline,
            (org_id, start, end),
            credit_type,
        )
        .await?;
        transaction.commit().await.map_err(map_db_error)?;
        Ok(report)
    }

    async fn get_platform_metrics(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<PlatformMetrics, RepositoryError> {
        let deadline = self.deadline()?;
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
        let report = platform::get_platform_metrics(&transaction, deadline, start, end).await?;
        transaction.commit().await.map_err(map_db_error)?;
        Ok(report)
    }

    async fn get_organization_timeseries(
        &self,
        org_id: Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        granularity: &str,
        credit_type: Option<&str>,
    ) -> Result<TimeSeriesMetrics, RepositoryError> {
        let deadline = self.deadline()?;
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
        let report = organization::get_organization_timeseries_with_client(
            &transaction,
            deadline,
            (org_id, start, end),
            granularity,
            credit_type,
        )
        .await?;
        transaction.commit().await.map_err(map_db_error)?;
        Ok(report)
    }

    async fn get_platform_timeseries(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        granularity: &str,
    ) -> Result<PlatformTimeSeriesMetrics, RepositoryError> {
        let deadline = self.deadline()?;
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
        let report =
            platform::get_platform_timeseries(&transaction, deadline, start, end, granularity)
                .await?;
        transaction.commit().await.map_err(map_db_error)?;
        Ok(report)
    }

    async fn get_billing_summary(&self) -> Result<BillingSummary, RepositoryError> {
        let deadline = self.deadline()?;
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
        let report = revenue::get_billing_summary(&transaction, deadline).await?;
        transaction.commit().await.map_err(map_db_error)?;
        Ok(report)
    }

    async fn get_model_revenue(
        &self,
        query: ModelRevenueQuery,
    ) -> Result<ModelRevenueReport, RepositoryError> {
        let deadline = self.deadline()?;
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
        let report = revenue::get_model_revenue(&transaction, deadline, query).await?;
        transaction.commit().await.map_err(map_db_error)?;
        Ok(report)
    }

    async fn get_org_revenue(
        &self,
        query: OrgRevenueQuery,
    ) -> Result<OrgRevenueReport, RepositoryError> {
        let deadline = self.deadline()?;
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
        let report = revenue::get_org_revenue(&transaction, deadline, query).await?;
        transaction.commit().await.map_err(map_db_error)?;
        Ok(report)
    }

    async fn get_model_consumption_timeseries(
        &self,
        query: ModelConsumptionTimeseriesQuery,
    ) -> Result<ModelConsumptionTimeseries, RepositoryError> {
        let deadline = self.deadline()?;
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
        let report =
            consumption::get_model_consumption_timeseries(&transaction, deadline, query).await?;
        transaction.commit().await.map_err(map_db_error)?;
        Ok(report)
    }

    async fn get_performance_timeseries(
        &self,
        query: PerformanceTimeseriesQuery,
    ) -> Result<PerformanceTimeseries, RepositoryError> {
        let deadline = self.deadline()?;
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
        let report = consumption::get_performance_timeseries(&transaction, deadline, query).await?;
        transaction.commit().await.map_err(map_db_error)?;
        Ok(report)
    }

    async fn get_revenue_density(
        &self,
        query: RevenueDensityQuery,
    ) -> Result<RevenueDensityReport, RepositoryError> {
        let deadline =
            reporting_query::reporting_deadline(REVENUE_DENSITY_STATEMENT_TIMEOUT, None)?;
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
        let report = revenue::get_revenue_density(&transaction, deadline, query).await?;
        transaction.commit().await.map_err(map_db_error)?;
        Ok(report)
    }
}
