use crate::repositories::utils::map_db_error;
use services::{
    common::RepositoryError,
    reporting_usage::{
        ReportingUsageSummaryFilters, ServiceApiKeySummary, ServiceDaySummary, ServiceNameSummary,
        ServiceUsageSummary, ServiceUsageTotals, ServiceWorkspaceSummary,
    },
};
use tokio_postgres::{GenericClient, Row};
use uuid::Uuid;

pub(super) async fn summarize_service_usage<C>(
    client: &C,
    filters: &ReportingUsageSummaryFilters,
) -> Result<ServiceUsageSummary, RepositoryError>
where
    C: GenericClient + Sync,
{
    let rows = client
        .query(
            r#"
            WITH filtered AS MATERIALIZED (
                SELECT usage_log.workspace_id, usage_log.api_key_id,
                       services.service_name,
                       DATE_TRUNC('day', usage_log.created_at) AS day,
                       usage_log.quantity, usage_log.total_cost
                FROM organization_service_usage_log AS usage_log
                INNER JOIN services ON services.id = usage_log.service_id
                WHERE usage_log.organization_id = $1
                  AND ($2::TIMESTAMPTZ IS NULL OR usage_log.created_at >= $2)
                  AND ($3::TIMESTAMPTZ IS NULL OR usage_log.created_at <= $3)
                  AND ($4::UUID IS NULL OR usage_log.workspace_id = $4)
                  AND ($5::UUID IS NULL OR usage_log.api_key_id = $5)
                  AND ($6::TEXT IS NULL OR services.service_name = $6)
            )
            SELECT
                CASE
                    WHEN GROUPING(workspace_id) = 0 THEN 'workspace'
                    WHEN GROUPING(api_key_id) = 0 THEN 'api_key'
                    WHEN GROUPING(service_name) = 0 THEN 'service'
                    WHEN GROUPING(day) = 0 THEN 'day'
                    ELSE 'totals'
                END AS dimension,
                workspace_id,
                api_key_id,
                service_name,
                TO_CHAR(day, 'YYYY-MM-DD') AS day,
                COUNT(*)::BIGINT AS usage_count,
                COALESCE(SUM(quantity), 0)::BIGINT AS quantity,
                COALESCE(SUM(total_cost), 0)::BIGINT AS total_cost
            FROM filtered
            GROUP BY GROUPING SETS (
                (), (workspace_id), (api_key_id), (service_name), (day)
            )
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

    let mut summary = ServiceUsageSummary::default();
    for row in &rows {
        let dimension: String = value(row, "dimension")?;
        let usage_count = value(row, "usage_count")?;
        let total_cost_nano_usd = value(row, "total_cost")?;
        match dimension.as_str() {
            "totals" => {
                summary.totals = ServiceUsageTotals {
                    usage_count,
                    total_cost_nano_usd,
                };
            }
            "workspace" => summary.by_workspace.push(ServiceWorkspaceSummary {
                workspace_id: required_uuid(row, "workspace_id")?,
                usage_count,
                total_cost_nano_usd,
            }),
            "api_key" => summary.by_api_key.push(ServiceApiKeySummary {
                api_key_id: required_uuid(row, "api_key_id")?,
                usage_count,
                total_cost_nano_usd,
            }),
            "service" => summary.by_service.push(ServiceNameSummary {
                service_name: required_string(row, "service_name")?,
                usage_count,
                quantity: value(row, "quantity")?,
                total_cost_nano_usd,
            }),
            "day" => summary.by_day.push(ServiceDaySummary {
                day: required_string(row, "day")?,
                usage_count,
                total_cost_nano_usd,
            }),
            other => {
                return Err(RepositoryError::DataConversionError(anyhow::anyhow!(
                    "unknown service summary dimension: {other}"
                )));
            }
        }
    }

    summary.by_workspace.sort_by(|left, right| {
        right
            .total_cost_nano_usd
            .cmp(&left.total_cost_nano_usd)
            .then_with(|| left.workspace_id.cmp(&right.workspace_id))
    });
    summary.by_api_key.sort_by(|left, right| {
        right
            .total_cost_nano_usd
            .cmp(&left.total_cost_nano_usd)
            .then_with(|| left.api_key_id.cmp(&right.api_key_id))
    });
    summary.by_service.sort_by(|left, right| {
        right
            .total_cost_nano_usd
            .cmp(&left.total_cost_nano_usd)
            .then_with(|| left.service_name.cmp(&right.service_name))
    });
    summary
        .by_day
        .sort_by(|left, right| left.day.cmp(&right.day));
    Ok(summary)
}

fn value<T>(row: &Row, column: &str) -> Result<T, RepositoryError>
where
    T: tokio_postgres::types::FromSqlOwned,
{
    row.try_get(column)
        .map_err(|error| RepositoryError::DataConversionError(error.into()))
}

fn required_uuid(row: &Row, column: &str) -> Result<Uuid, RepositoryError> {
    value::<Option<Uuid>>(row, column)?
        .ok_or_else(|| RepositoryError::DataConversionError(anyhow::anyhow!("missing {column}")))
}

fn required_string(row: &Row, column: &str) -> Result<String, RepositoryError> {
    value::<Option<String>>(row, column)?
        .ok_or_else(|| RepositoryError::DataConversionError(anyhow::anyhow!("missing {column}")))
}
