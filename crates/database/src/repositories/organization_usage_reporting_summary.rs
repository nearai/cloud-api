use crate::repositories::utils::map_db_error;
use services::{
    common::RepositoryError,
    reporting_usage::{
        InferenceApiKeySummary, InferenceDaySummary, InferenceModelSummary, InferenceUsageSummary,
        InferenceUsageTotals, InferenceWorkspaceSummary, ReportingUsageSummaryFilters,
    },
};
use tokio_postgres::{GenericClient, Row};
use uuid::Uuid;

pub(super) async fn summarize_inference_usage<C>(
    client: &C,
    filters: &ReportingUsageSummaryFilters,
) -> Result<InferenceUsageSummary, RepositoryError>
where
    C: GenericClient + Sync,
{
    let rows = client
        .query(
            r#"
            WITH filtered AS MATERIALIZED (
                SELECT usage_log.workspace_id, usage_log.api_key_id, usage_log.model_name,
                       DATE_TRUNC('day', usage_log.created_at) AS day,
                       usage_log.input_tokens, usage_log.output_tokens,
                       usage_log.cache_read_tokens, usage_log.total_tokens,
                       CASE WHEN $8::TEXT IS NULL THEN
                           usage_log.total_cost - COALESCE((
                               SELECT SUM(amount)::BIGINT FROM usage_credit_adjustments adjustment
                               WHERE adjustment.inference_usage_id = usage_log.id
                           ), 0)
                       ELSE allocation.amount END AS total_cost
                FROM organization_usage_log usage_log
                LEFT JOIN LATERAL (
                    SELECT original.amount - COALESCE((
                        SELECT SUM(reversal.amount)::BIGINT
                        FROM usage_credit_allocation_reversals reversal
                        WHERE reversal.allocation_id = original.id
                    ), 0) AS amount
                    FROM usage_credit_allocations original
                    WHERE original.inference_usage_id = usage_log.id
                      AND original.credit_type = $8
                ) allocation ON true
                WHERE usage_log.organization_id = $1
                  AND ($2::TIMESTAMPTZ IS NULL OR usage_log.created_at >= $2)
                  AND ($3::TIMESTAMPTZ IS NULL OR usage_log.created_at <= $3)
                  AND ($4::UUID IS NULL OR usage_log.workspace_id = $4)
                  AND ($5::UUID IS NULL OR usage_log.api_key_id = $5)
                  AND ($6::TEXT IS NULL OR usage_log.model_name = $6)
                  AND ($7::TEXT IS NULL OR usage_log.inference_type = $7)
                  AND ($8::TEXT IS NULL OR allocation.amount > 0)
            )
            SELECT
                CASE
                    WHEN GROUPING(workspace_id) = 0 THEN 'workspace'
                    WHEN GROUPING(api_key_id) = 0 THEN 'api_key'
                    WHEN GROUPING(model_name) = 0 THEN 'model'
                    WHEN GROUPING(day) = 0 THEN 'day'
                    ELSE 'totals'
                END AS dimension,
                workspace_id,
                api_key_id,
                model_name,
                TO_CHAR(day, 'YYYY-MM-DD') AS day,
                COUNT(*)::BIGINT AS request_count,
                COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens,
                COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens,
                COALESCE(SUM(cache_read_tokens), 0)::BIGINT AS cache_read_tokens,
                COALESCE(SUM(total_tokens), 0)::BIGINT AS total_tokens,
                COALESCE(SUM(total_cost), 0)::BIGINT AS total_cost
            FROM filtered
            GROUP BY GROUPING SETS (
                (), (workspace_id), (api_key_id), (model_name), (day)
            )
            "#,
            &[
                &filters.organization_id,
                &filters.start_time,
                &filters.end_time,
                &filters.workspace_id,
                &filters.api_key_id,
                &filters.model,
                &filters.inference_type,
                &filters.credit_type,
            ],
        )
        .await
        .map_err(map_db_error)?;

    let mut summary = InferenceUsageSummary::default();
    for row in &rows {
        let dimension: String = value(row, "dimension")?;
        let request_count = value(row, "request_count")?;
        let total_cost_nano_usd = value(row, "total_cost")?;
        match dimension.as_str() {
            "totals" => {
                summary.totals = InferenceUsageTotals {
                    request_count,
                    input_tokens: value(row, "input_tokens")?,
                    output_tokens: value(row, "output_tokens")?,
                    cache_read_tokens: value(row, "cache_read_tokens")?,
                    total_tokens: value(row, "total_tokens")?,
                    total_cost_nano_usd,
                };
            }
            "workspace" => summary.by_workspace.push(InferenceWorkspaceSummary {
                workspace_id: required_uuid(row, "workspace_id")?,
                request_count,
                total_cost_nano_usd,
            }),
            "api_key" => summary.by_api_key.push(InferenceApiKeySummary {
                api_key_id: required_uuid(row, "api_key_id")?,
                request_count,
                total_cost_nano_usd,
            }),
            "model" => summary.by_model.push(InferenceModelSummary {
                model: required_string(row, "model_name")?,
                request_count,
                input_tokens: value(row, "input_tokens")?,
                output_tokens: value(row, "output_tokens")?,
                cache_read_tokens: value(row, "cache_read_tokens")?,
                total_tokens: value(row, "total_tokens")?,
                total_cost_nano_usd,
            }),
            "day" => summary.by_day.push(InferenceDaySummary {
                day: required_string(row, "day")?,
                request_count,
                input_tokens: value(row, "input_tokens")?,
                output_tokens: value(row, "output_tokens")?,
                cache_read_tokens: value(row, "cache_read_tokens")?,
                total_tokens: value(row, "total_tokens")?,
                total_cost_nano_usd,
            }),
            other => {
                return Err(RepositoryError::DataConversionError(anyhow::anyhow!(
                    "unknown inference summary dimension: {other}"
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
    summary.by_model.sort_by(|left, right| {
        right
            .total_cost_nano_usd
            .cmp(&left.total_cost_nano_usd)
            .then_with(|| left.model.cmp(&right.model))
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
