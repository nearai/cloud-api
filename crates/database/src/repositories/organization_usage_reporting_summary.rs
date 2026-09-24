use super::analytics::hour_range::hour_range_inclusive;
use crate::repositories::utils::map_db_error;
use chrono::{DateTime, TimeDelta, Utc};
use services::{
    common::RepositoryError,
    reporting_usage::{
        InferenceApiKeySummary, InferenceDaySummary, InferenceModelSummary, InferenceUsageSummary,
        InferenceUsageTotals, InferenceWorkspaceSummary, ReportingUsageSummaryFilters,
        ReportingUsageSummarySource,
    },
};
use tokio_postgres::{GenericClient, Row};
use uuid::Uuid;

/// `usage_hourly` for organization `$1`; `$2`/`$3` are the served inclusive bounds (whole
/// hours, see `served_range`), `$4..$7` the optional filters.
const HOURLY_INFERENCE_SUMMARY: &str = r#"
    WITH filtered AS MATERIALIZED (
        SELECT uh.workspace_id, uh.api_key_id, uh.model_name,
               DATE_TRUNC('day', uh.hour) AS day,
               uh.request_count, uh.input_tokens, uh.output_tokens,
               uh.cache_read_tokens, uh.total_tokens, uh.total_cost
        FROM usage_hourly uh
        WHERE uh.organization_id = $1
          AND ($2::TIMESTAMPTZ IS NULL OR uh.hour >= $2)
          AND ($3::TIMESTAMPTZ IS NULL OR uh.hour <= $3)
          AND ($4::UUID IS NULL OR uh.workspace_id = $4)
          AND ($5::UUID IS NULL OR uh.api_key_id = $5)
          AND ($6::TEXT IS NULL OR uh.model_name = $6)
          AND ($7::TEXT IS NULL OR uh.inference_type = $7)
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
        COALESCE(SUM(request_count), 0)::BIGINT AS request_count,
        COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens,
        COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens,
        COALESCE(SUM(cache_read_tokens), 0)::BIGINT AS cache_read_tokens,
        COALESCE(SUM(total_tokens), 0)::BIGINT AS total_tokens,
        COALESCE(SUM(total_cost), 0)::BIGINT AS total_cost
    FROM filtered
    GROUP BY GROUPING SETS (
        (), (workspace_id), (api_key_id), (model_name), (day)
    )
"#;

// ponytail: credit_type filters read raw organization_usage_log because allocations settle after
// posting (settle_unfunded_usage), so usage_hourly cannot carry them. Ceiling: a filtered window
// must finish within this repository's statement timeout as a per-org raw scan. Upgrade path: an
// allocation-aware aggregate recomputed after settlement, once settlement has a completion signal.
const CREDIT_TYPE_INFERENCE_SUMMARY: &str = r#"
    WITH filtered AS MATERIALIZED (
        SELECT usage_log.workspace_id, usage_log.api_key_id, usage_log.model_name,
               DATE_TRUNC('day', usage_log.created_at) AS day,
               usage_log.input_tokens, usage_log.output_tokens,
               usage_log.cache_read_tokens, usage_log.total_tokens,
               allocation.amount AS total_cost
        FROM organization_usage_log usage_log
        LEFT JOIN LATERAL (
            SELECT COALESCE(SUM(original.amount), 0)::BIGINT AS amount
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
          AND allocation.amount > 0
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
"#;

/// Inclusive `(start_time, end_time)` a summary serves; `None` only for an exact reader
/// given an open bound.
pub(super) type ServedRange = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// The inclusive range a usage summary serves (spec §6.1). Without `credit_type`, the
/// inference part reads `usage_hourly`, so the request range widens to whole UTC hours,
/// `[trunc_hour(start), trunc_hour(end) + 1h)`. The range is kept inclusive (`<= end`) by
/// ending 1 µs, PostgreSQL's timestamp resolution, before that hour, and for `source=all`
/// the service part is queried over the same range. Service-only and `credit_type`
/// summaries stay exact.
pub(super) fn served_range(
    filters: &ReportingUsageSummaryFilters,
) -> Result<ServedRange, RepositoryError> {
    if filters.source == ReportingUsageSummarySource::Service || filters.credit_type.is_some() {
        return Ok((filters.start_time, filters.end_time));
    }
    let (Some(start), Some(end)) = (filters.start_time, filters.end_time) else {
        return Err(RepositoryError::ValidationFailed(
            "an hourly usage summary needs start_time and end_time".to_string(),
        ));
    };
    let (start, end_exclusive) = hour_range_inclusive(start, end);
    Ok((
        Some(start),
        Some(end_exclusive - TimeDelta::microseconds(1)),
    ))
}

pub(super) async fn summarize_inference_usage<C>(
    client: &C,
    filters: &ReportingUsageSummaryFilters,
) -> Result<InferenceUsageSummary, RepositoryError>
where
    C: GenericClient + Sync,
{
    let rows = match filters.credit_type.as_deref() {
        None => {
            client
                .query(
                    HOURLY_INFERENCE_SUMMARY,
                    &[
                        &filters.organization_id,
                        &filters.start_time,
                        &filters.end_time,
                        &filters.workspace_id,
                        &filters.api_key_id,
                        &filters.model,
                        &filters.inference_type,
                    ],
                )
                .await
        }
        Some(credit_type) => {
            client
                .query(
                    CREDIT_TYPE_INFERENCE_SUMMARY,
                    &[
                        &filters.organization_id,
                        &filters.start_time,
                        &filters.end_time,
                        &filters.workspace_id,
                        &filters.api_key_id,
                        &filters.model,
                        &filters.inference_type,
                        &credit_type,
                    ],
                )
                .await
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeDelta, Utc};
    use services::reporting_usage::ReportingUsageSummarySource;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn filters(
        source: ReportingUsageSummarySource,
        credit_type: Option<&str>,
    ) -> ReportingUsageSummaryFilters {
        ReportingUsageSummaryFilters {
            organization_id: Uuid::nil(),
            start_time: Some(t("2026-07-02T00:15:00Z")),
            end_time: Some(t("2026-07-02T00:20:00Z")),
            workspace_id: None,
            api_key_id: None,
            model: None,
            inference_type: None,
            service_name: None,
            credit_type: credit_type.map(str::to_string),
            source,
            deadline: None,
        }
    }

    #[test]
    fn hourly_summaries_serve_whole_hours_with_an_inclusive_end() {
        for source in [
            ReportingUsageSummarySource::All,
            ReportingUsageSummarySource::Inference,
        ] {
            assert_eq!(
                served_range(&filters(source, None)).unwrap(),
                (
                    Some(t("2026-07-02T00:00:00Z")),
                    Some(t("2026-07-02T01:00:00Z") - TimeDelta::microseconds(1))
                )
            );
        }
    }

    #[test]
    fn exact_summaries_keep_the_requested_range() {
        let exact = (
            Some(t("2026-07-02T00:15:00Z")),
            Some(t("2026-07-02T00:20:00Z")),
        );
        assert_eq!(
            served_range(&filters(ReportingUsageSummarySource::Service, None)).unwrap(),
            exact
        );
        assert_eq!(
            served_range(&filters(ReportingUsageSummarySource::All, Some("payment"))).unwrap(),
            exact
        );
    }

    #[test]
    fn an_unbounded_hourly_summary_is_rejected() {
        let mut open = filters(ReportingUsageSummarySource::All, None);
        open.start_time = None;
        assert!(served_range(&open).is_err());
    }
}
