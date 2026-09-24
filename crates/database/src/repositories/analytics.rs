//! Analytics repository implementation for enterprise dashboard queries.
//!
//! All costs use fixed scale 9 (nano-dollars) and USD currency. This impl owns client
//! acquisition, transactions and timeouts; report SQL lives in one file per report family.

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
use std::time::Duration;
use uuid::Uuid;

/// PostgreSQL implementation of the analytics repository
pub struct PgAnalyticsRepository {
    pool: DbPool,
    filtered_metrics_timeout: Duration,
}

impl PgAnalyticsRepository {
    pub fn new(pool: DbPool) -> Self {
        Self::with_filtered_metrics_timeout(
            pool,
            reporting_query::DEFAULT_REPORTING_STATEMENT_TIMEOUT,
        )
    }

    pub fn with_filtered_metrics_timeout(pool: DbPool, filtered_metrics_timeout: Duration) -> Self {
        Self {
            pool,
            filtered_metrics_timeout,
        }
    }
}

/// Convert nano-dollars (scale 9) to USD
fn nano_to_usd(nano: i64) -> f64 {
    nano as f64 / 1_000_000_000.0
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
        let deadline = if credit_type.is_some() {
            Some(reporting_query::reporting_deadline(
                self.filtered_metrics_timeout,
                None,
            )?)
        } else {
            None
        };
        let mut client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;
        if let Some(deadline) = deadline {
            let transaction = client
                .build_transaction()
                .read_only(true)
                .start()
                .await
                .map_err(map_db_error)?;
            let result = organization::get_organization_metrics_with_client(
                &*transaction,
                (org_id, start, end),
                credit_type,
                Some((&transaction, deadline)),
            )
            .await?;
            transaction.commit().await.map_err(map_db_error)?;
            Ok(result)
        } else {
            organization::get_organization_metrics_with_client(
                &**client,
                (org_id, start, end),
                credit_type,
                None,
            )
            .await
        }
    }

    async fn get_platform_metrics(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<PlatformMetrics, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;
        platform::get_platform_metrics(&client, start, end).await
    }

    async fn get_organization_timeseries(
        &self,
        org_id: Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        granularity: &str,
        credit_type: Option<&str>,
    ) -> Result<TimeSeriesMetrics, RepositoryError> {
        let deadline = if credit_type.is_some() {
            Some(reporting_query::reporting_deadline(
                self.filtered_metrics_timeout,
                None,
            )?)
        } else {
            None
        };
        let mut client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;
        if let Some(deadline) = deadline {
            let transaction = client
                .build_transaction()
                .read_only(true)
                .start()
                .await
                .map_err(map_db_error)?;
            let result = organization::get_organization_timeseries_with_client(
                &*transaction,
                (org_id, start, end),
                granularity,
                credit_type,
                Some((&transaction, deadline)),
            )
            .await?;
            transaction.commit().await.map_err(map_db_error)?;
            Ok(result)
        } else {
            organization::get_organization_timeseries_with_client(
                &**client,
                (org_id, start, end),
                granularity,
                credit_type,
                None,
            )
            .await
        }
    }

    async fn get_platform_timeseries(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        granularity: &str,
    ) -> Result<PlatformTimeSeriesMetrics, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;
        platform::get_platform_timeseries(&client, start, end, granularity).await
    }

    async fn get_billing_summary(&self) -> Result<BillingSummary, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;
        revenue::get_billing_summary(&client).await
    }

    async fn get_model_revenue(
        &self,
        query: ModelRevenueQuery,
    ) -> Result<ModelRevenueReport, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;
        revenue::get_model_revenue(&client, query).await
    }

    async fn get_org_revenue(
        &self,
        query: OrgRevenueQuery,
    ) -> Result<OrgRevenueReport, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;
        revenue::get_org_revenue(&client, query).await
    }

    async fn get_model_consumption_timeseries(
        &self,
        query: ModelConsumptionTimeseriesQuery,
    ) -> Result<ModelConsumptionTimeseries, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;
        consumption::get_model_consumption_timeseries(&client, query).await
    }

    async fn get_performance_timeseries(
        &self,
        query: PerformanceTimeseriesQuery,
    ) -> Result<PerformanceTimeseries, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;
        consumption::get_performance_timeseries(&client, query).await
    }

    async fn get_revenue_density(
        &self,
        query: RevenueDensityQuery,
    ) -> Result<RevenueDensityReport, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;
        revenue::get_revenue_density(&client, query).await
    }
}
