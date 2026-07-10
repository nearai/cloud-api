use crate::repositories::{utils::map_db_error, OrganizationServiceUsageRepository};
use crate::retry_db;
use anyhow::{Context, Result};
use services::{
    common::RepositoryError,
    reporting_usage::{
        ReportingUsageSummaryFilters, ServiceApiKeySummary, ServiceDaySummary, ServiceNameSummary,
        ServiceUsageSummary, ServiceUsageSummaryRepository, ServiceUsageTotals,
        ServiceWorkspaceSummary,
    },
};
use tokio_postgres::Row;

impl OrganizationServiceUsageRepository {
    pub async fn summarize_service_usage_report(
        &self,
        filters: &ReportingUsageSummaryFilters,
    ) -> Result<ServiceUsageSummary> {
        let summary = retry_db!("summarize_service_usage_report", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let totals = client
                .query_one(
                    r#"
                    SELECT COUNT(*)::bigint,
                           COALESCE(SUM(usage_log.total_cost), 0)::bigint
                    FROM organization_service_usage_log AS usage_log
                    INNER JOIN services ON services.id = usage_log.service_id
                    WHERE usage_log.organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR usage_log.created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR usage_log.created_at <= $3)
                      AND ($4::UUID IS NULL OR usage_log.workspace_id = $4)
                      AND ($5::UUID IS NULL OR usage_log.api_key_id = $5)
                      AND ($6::TEXT IS NULL OR services.service_name = $6)
                    "#,
                    &[
                        &filters.organization_id,
                        &filters.start_time,
                        &filters.end_time,
                        &filters.workspace_id,
                        &filters.api_key_id,
                        &filters.service_name,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            let by_workspace = client
                .query(
                    r#"
                    SELECT usage_log.workspace_id, COUNT(*)::bigint,
                           COALESCE(SUM(usage_log.total_cost), 0)::bigint
                    FROM organization_service_usage_log AS usage_log
                    INNER JOIN services ON services.id = usage_log.service_id
                    WHERE usage_log.organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR usage_log.created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR usage_log.created_at <= $3)
                      AND ($4::UUID IS NULL OR usage_log.workspace_id = $4)
                      AND ($5::UUID IS NULL OR usage_log.api_key_id = $5)
                      AND ($6::TEXT IS NULL OR services.service_name = $6)
                    GROUP BY usage_log.workspace_id
                    ORDER BY 3 DESC, usage_log.workspace_id ASC
                    "#,
                    &[
                        &filters.organization_id,
                        &filters.start_time,
                        &filters.end_time,
                        &filters.workspace_id,
                        &filters.api_key_id,
                        &filters.service_name,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            let by_api_key = client
                .query(
                    r#"
                    SELECT usage_log.api_key_id, COUNT(*)::bigint,
                           COALESCE(SUM(usage_log.total_cost), 0)::bigint
                    FROM organization_service_usage_log AS usage_log
                    INNER JOIN services ON services.id = usage_log.service_id
                    WHERE usage_log.organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR usage_log.created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR usage_log.created_at <= $3)
                      AND ($4::UUID IS NULL OR usage_log.workspace_id = $4)
                      AND ($5::UUID IS NULL OR usage_log.api_key_id = $5)
                      AND ($6::TEXT IS NULL OR services.service_name = $6)
                    GROUP BY usage_log.api_key_id
                    ORDER BY 3 DESC, usage_log.api_key_id ASC
                    "#,
                    &[
                        &filters.organization_id,
                        &filters.start_time,
                        &filters.end_time,
                        &filters.workspace_id,
                        &filters.api_key_id,
                        &filters.service_name,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            let by_service = client
                .query(
                    r#"
                    SELECT services.service_name, COUNT(*)::bigint,
                           COALESCE(SUM(usage_log.quantity), 0)::bigint,
                           COALESCE(SUM(usage_log.total_cost), 0)::bigint
                    FROM organization_service_usage_log AS usage_log
                    INNER JOIN services ON services.id = usage_log.service_id
                    WHERE usage_log.organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR usage_log.created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR usage_log.created_at <= $3)
                      AND ($4::UUID IS NULL OR usage_log.workspace_id = $4)
                      AND ($5::UUID IS NULL OR usage_log.api_key_id = $5)
                      AND ($6::TEXT IS NULL OR services.service_name = $6)
                    GROUP BY services.service_name
                    ORDER BY 4 DESC, services.service_name ASC
                    "#,
                    &[
                        &filters.organization_id,
                        &filters.start_time,
                        &filters.end_time,
                        &filters.workspace_id,
                        &filters.api_key_id,
                        &filters.service_name,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            let by_day = client
                .query(
                    r#"
                    SELECT TO_CHAR(DATE_TRUNC('day', usage_log.created_at), 'YYYY-MM-DD'),
                           COUNT(*)::bigint,
                           COALESCE(SUM(usage_log.total_cost), 0)::bigint
                    FROM organization_service_usage_log AS usage_log
                    INNER JOIN services ON services.id = usage_log.service_id
                    WHERE usage_log.organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR usage_log.created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR usage_log.created_at <= $3)
                      AND ($4::UUID IS NULL OR usage_log.workspace_id = $4)
                      AND ($5::UUID IS NULL OR usage_log.api_key_id = $5)
                      AND ($6::TEXT IS NULL OR services.service_name = $6)
                    GROUP BY DATE_TRUNC('day', usage_log.created_at)
                    ORDER BY 1 ASC
                    "#,
                    &[
                        &filters.organization_id,
                        &filters.start_time,
                        &filters.end_time,
                        &filters.workspace_id,
                        &filters.api_key_id,
                        &filters.service_name,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            Ok::<_, RepositoryError>(ServiceUsageSummary {
                totals: ServiceUsageTotals {
                    usage_count: totals.get(0),
                    total_cost_nano_usd: totals.get(1),
                },
                by_workspace: by_workspace.iter().map(workspace_from_row).collect(),
                by_api_key: by_api_key.iter().map(api_key_from_row).collect(),
                by_service: by_service.iter().map(service_from_row).collect(),
                by_day: by_day.iter().map(day_from_row).collect(),
            })
        })?;

        Ok(summary)
    }
}

#[async_trait::async_trait]
impl ServiceUsageSummaryRepository for OrganizationServiceUsageRepository {
    async fn summarize_service_usage(
        &self,
        filters: &ReportingUsageSummaryFilters,
    ) -> Result<ServiceUsageSummary> {
        self.summarize_service_usage_report(filters).await
    }
}

fn workspace_from_row(row: &Row) -> ServiceWorkspaceSummary {
    ServiceWorkspaceSummary {
        workspace_id: row.get(0),
        usage_count: row.get(1),
        total_cost_nano_usd: row.get(2),
    }
}

fn api_key_from_row(row: &Row) -> ServiceApiKeySummary {
    ServiceApiKeySummary {
        api_key_id: row.get(0),
        usage_count: row.get(1),
        total_cost_nano_usd: row.get(2),
    }
}

fn service_from_row(row: &Row) -> ServiceNameSummary {
    ServiceNameSummary {
        service_name: row.get(0),
        usage_count: row.get(1),
        quantity: row.get(2),
        total_cost_nano_usd: row.get(3),
    }
}

fn day_from_row(row: &Row) -> ServiceDaySummary {
    ServiceDaySummary {
        day: row.get(0),
        usage_count: row.get(1),
        total_cost_nano_usd: row.get(2),
    }
}
