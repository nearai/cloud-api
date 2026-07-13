use super::{
    organization_service_usage_reporting_summary::summarize_service_usage,
    organization_usage_reporting_summary::summarize_inference_usage, utils::map_db_error,
};
use crate::{pool::DbPool, retry_db};
use anyhow::{Context, Result};
use services::{
    common::RepositoryError,
    reporting_usage::{
        ReportingUsageSummary, ReportingUsageSummaryFilters, ReportingUsageSummaryRepository,
        ReportingUsageSummarySource,
    },
};
use std::time::Duration;
use tokio_postgres::IsolationLevel;

pub struct PostgresReportingUsageSummaryRepository {
    pool: DbPool,
    statement_timeout: Duration,
}

impl PostgresReportingUsageSummaryRepository {
    pub fn new(pool: DbPool) -> Self {
        Self {
            pool,
            statement_timeout: super::reporting_query::DEFAULT_REPORTING_STATEMENT_TIMEOUT,
        }
    }

    pub fn with_statement_timeout(pool: DbPool, statement_timeout: Duration) -> Self {
        Self {
            pool,
            statement_timeout,
        }
    }

    async fn summarize_report(
        &self,
        filters: &ReportingUsageSummaryFilters,
    ) -> Result<ReportingUsageSummary> {
        let deadline =
            super::reporting_query::reporting_deadline(self.statement_timeout, filters.deadline)?;
        let summary = retry_db!("summarize_reporting_usage", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            let transaction = client
                .build_transaction()
                .isolation_level(IsolationLevel::RepeatableRead)
                .read_only(true)
                .start()
                .await
                .map_err(map_db_error)?;
            let inference = match filters.source {
                ReportingUsageSummarySource::All | ReportingUsageSummarySource::Inference => {
                    super::reporting_query::configure_reporting_transaction(
                        &transaction,
                        super::reporting_query::remaining_statement_timeout(deadline)?,
                    )
                    .await?;
                    summarize_inference_usage(&*transaction, filters).await?
                }
                ReportingUsageSummarySource::Service => Default::default(),
            };
            let service = match filters.source {
                ReportingUsageSummarySource::All | ReportingUsageSummarySource::Service => {
                    super::reporting_query::configure_reporting_transaction(
                        &transaction,
                        super::reporting_query::remaining_statement_timeout(deadline)?,
                    )
                    .await?;
                    summarize_service_usage(&*transaction, filters).await?
                }
                ReportingUsageSummarySource::Inference => Default::default(),
            };

            transaction.commit().await.map_err(map_db_error)?;
            Ok::<_, RepositoryError>(ReportingUsageSummary { inference, service })
        })?;
        Ok(summary)
    }
}

#[async_trait::async_trait]
impl ReportingUsageSummaryRepository for PostgresReportingUsageSummaryRepository {
    async fn summarize_usage(
        &self,
        filters: &ReportingUsageSummaryFilters,
    ) -> Result<ReportingUsageSummary> {
        self.summarize_report(filters).await
    }
}
