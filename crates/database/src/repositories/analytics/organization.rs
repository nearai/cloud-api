//! Organization metrics and timeseries (admin and customer routes).

use super::{arm, nano_to_usd};
use crate::repositories::utils::map_db_error;
use chrono::{DateTime, Utc};
use services::admin::{
    ApiKeyMetrics, MetricsSummary, ModelMetrics, OrganizationMetrics, TimeSeriesMetrics,
    TimeSeriesPoint, WorkspaceMetrics,
};
use services::common::RepositoryError;
use std::time::Instant;
use tokio_postgres::Transaction;
use uuid::Uuid;

// Keep one row per inference request, even when posting and settlement both
// allocate the requested credit type. Unfiltered costs retain historical usage.
const ORGANIZATION_USAGE_METRICS_CTE: &str = r#"
    WITH metric_usage AS (
        SELECT ul.*, CASE WHEN $4::TEXT IS NULL THEN ul.total_cost
                          ELSE allocation.amount END AS filtered_cost
        FROM organization_usage_log ul
        LEFT JOIN LATERAL (
            SELECT SUM(a.amount)::BIGINT AS amount
            FROM usage_credit_allocations a
            WHERE a.inference_usage_id = ul.id AND a.credit_type = $4
        ) allocation ON true
        WHERE ul.organization_id = $1 AND ul.created_at >= $2 AND ul.created_at < $3
          AND ($4::TEXT IS NULL OR allocation.amount > 0)
    )
"#;

// The default path must not depend on custom-plan constant folding: generic
// prepared plans must also avoid per-request allocation lookups.
const UNFILTERED_ORGANIZATION_USAGE_METRICS_CTE: &str = r#"
    WITH metric_usage AS (
        SELECT ul.*, ul.total_cost AS filtered_cost
        FROM organization_usage_log ul
        WHERE ul.organization_id = $1 AND ul.created_at >= $2 AND ul.created_at < $3
          -- Keep the fourth binding shared with the filtered query variant.
          AND $4::TEXT IS NULL
    )
"#;

fn organization_usage_metrics_cte(credit_type: Option<&str>) -> &'static str {
    match credit_type {
        Some(_) => ORGANIZATION_USAGE_METRICS_CTE,
        None => UNFILTERED_ORGANIZATION_USAGE_METRICS_CTE,
    }
}

pub(super) async fn get_organization_metrics_with_client(
    tx: &Transaction<'_>,
    deadline: Instant,
    window: (Uuid, DateTime<Utc>, DateTime<Utc>),
    credit_type: Option<&str>,
) -> Result<OrganizationMetrics, RepositoryError> {
    let (org_id, start, end) = window;
    let usage_cte = organization_usage_metrics_cte(credit_type);
    // Get organization name
    arm(tx, deadline).await?;
    let org_row = tx
        .query_opt("SELECT name FROM organizations WHERE id = $1", &[&org_id])
        .await
        .map_err(map_db_error)?
        .ok_or_else(|| RepositoryError::NotFound(format!("Organization {org_id}")))?;
    let org_name: String = org_row.get(0);

    // Get summary metrics including unique API keys
    arm(tx, deadline).await?;
    let summary_row = tx
        .query_one(
            &format!(
                r#"{usage_cte}
                SELECT
                    COUNT(*)::bigint as requests,
                    COALESCE(SUM(input_tokens), 0)::bigint as input_tokens,
                    COALESCE(SUM(output_tokens), 0)::bigint as output_tokens,
                    COALESCE(SUM(cache_read_tokens), 0)::bigint as cache_read_tokens,
                    COALESCE(SUM(filtered_cost), 0)::bigint as cost_nano,
                    COUNT(DISTINCT api_key_id)::bigint as unique_api_keys
                FROM metric_usage
                "#
            ),
            &[&org_id, &start, &end, &credit_type],
        )
        .await
        .map_err(map_db_error)?;

    let summary = MetricsSummary {
        total_requests: summary_row.get::<_, i64>(0),
        total_input_tokens: summary_row.get::<_, i64>(1),
        total_output_tokens: summary_row.get::<_, i64>(2),
        total_cache_read_tokens: summary_row.get::<_, i64>(3),
        total_cost_usd: nano_to_usd(summary_row.get::<_, i64>(4)),
        unique_api_keys: summary_row.get::<_, i64>(5),
    };

    // Get metrics by workspace
    arm(tx, deadline).await?;
    let workspace_rows = tx
        .query(
            &format!(
                r#"{usage_cte}
                SELECT
                    w.id as workspace_id,
                    w.name as workspace_name,
                    COUNT(ul.id)::bigint as requests,
                    COALESCE(SUM(ul.input_tokens), 0)::bigint as input_tokens,
                    COALESCE(SUM(ul.output_tokens), 0)::bigint as output_tokens,
                    COALESCE(SUM(ul.cache_read_tokens), 0)::bigint as cache_read_tokens,
                    COALESCE(SUM(ul.filtered_cost), 0)::bigint as cost_nano
                FROM workspaces w
                LEFT JOIN metric_usage ul ON ul.workspace_id = w.id
                WHERE w.organization_id = $1
                GROUP BY w.id, w.name
                ORDER BY requests DESC
                "#
            ),
            &[&org_id, &start, &end, &credit_type],
        )
        .await
        .map_err(map_db_error)?;

    let by_workspace: Vec<WorkspaceMetrics> = workspace_rows
        .iter()
        .map(|row| WorkspaceMetrics {
            workspace_id: row.get(0),
            workspace_name: row.get(1),
            requests: row.get(2),
            input_tokens: row.get(3),
            output_tokens: row.get(4),
            cache_read_tokens: row.get(5),
            cost_usd: nano_to_usd(row.get::<_, i64>(6)),
        })
        .collect();

    // Get metrics by API key
    arm(tx, deadline).await?;
    let api_key_rows = tx
        .query(
            &format!(
                r#"{usage_cte}
                SELECT
                    ak.id as api_key_id,
                    ak.name as api_key_name,
                    COUNT(ul.id)::bigint as requests,
                    COALESCE(SUM(ul.filtered_cost), 0)::bigint as cost_nano
                FROM api_keys ak
                LEFT JOIN metric_usage ul ON ul.api_key_id = ak.id
                WHERE ak.workspace_id IN (
                    SELECT id FROM workspaces WHERE organization_id = $1
                )
                GROUP BY ak.id, ak.name
                ORDER BY requests DESC
                "#
            ),
            &[&org_id, &start, &end, &credit_type],
        )
        .await
        .map_err(map_db_error)?;

    let by_api_key: Vec<ApiKeyMetrics> = api_key_rows
        .iter()
        .map(|row| ApiKeyMetrics {
            api_key_id: row.get(0),
            api_key_name: row.get(1),
            requests: row.get(2),
            cost_usd: nano_to_usd(row.get::<_, i64>(3)),
        })
        .collect();

    // Get metrics by model (including latency metrics: TTFT and ITL)
    arm(tx, deadline).await?;
    let model_rows = tx
            .query(
                &format!(r#"{usage_cte}
                SELECT
                    ul.model_name,
                    COUNT(*)::bigint as requests,
                    COALESCE(SUM(ul.input_tokens), 0)::bigint as input_tokens,
                    COALESCE(SUM(ul.output_tokens), 0)::bigint as output_tokens,
                    COALESCE(SUM(ul.cache_read_tokens), 0)::bigint as cache_read_tokens,
                    COALESCE(SUM(ul.filtered_cost), 0)::bigint as cost_nano,
                    AVG(ul.ttft_ms)::double precision as avg_ttft_ms,
                    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision as p95_ttft_ms,
                    AVG(ul.avg_itl_ms)::double precision as avg_itl_ms,
                    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ul.avg_itl_ms)::double precision as p95_itl_ms
                FROM metric_usage ul
                GROUP BY ul.model_name
                ORDER BY requests DESC
                "#),
                &[&org_id, &start, &end, &credit_type],
            )
            .await
            .map_err(map_db_error)?;

    let by_model: Vec<ModelMetrics> = model_rows
        .iter()
        .map(|row| ModelMetrics {
            model_name: row.get(0),
            requests: row.get(1),
            input_tokens: row.get(2),
            output_tokens: row.get(3),
            cache_read_tokens: row.get(4),
            cost_usd: nano_to_usd(row.get::<_, i64>(5)),
            avg_ttft_ms: row.get::<_, Option<f64>>(6),
            p95_ttft_ms: row.get::<_, Option<f64>>(7),
            avg_itl_ms: row.get::<_, Option<f64>>(8),
            p95_itl_ms: row.get::<_, Option<f64>>(9),
        })
        .collect();

    Ok(OrganizationMetrics {
        organization_id: org_id,
        organization_name: org_name,
        period_start: start,
        period_end: end,
        summary,
        by_workspace,
        by_api_key,
        by_model,
    })
}

pub(super) async fn get_organization_timeseries_with_client(
    tx: &Transaction<'_>,
    deadline: Instant,
    window: (Uuid, DateTime<Utc>, DateTime<Utc>),
    granularity: &str,
    credit_type: Option<&str>,
) -> Result<TimeSeriesMetrics, RepositoryError> {
    let (org_id, start, end) = window;
    let usage_cte = organization_usage_metrics_cte(credit_type);
    // Get organization name
    arm(tx, deadline).await?;
    let org_row = tx
        .query_opt("SELECT name FROM organizations WHERE id = $1", &[&org_id])
        .await
        .map_err(map_db_error)?
        .ok_or_else(|| RepositoryError::NotFound(format!("Organization {org_id}")))?;
    let org_name: String = org_row.get(0);

    // Determine date truncation based on granularity
    let date_trunc = match granularity {
        "hour" => "hour",
        "week" => "week",
        _ => "day", // default to day
    };

    // Get time series data
    let query = format!(
        r#"{usage_cte}
            SELECT
                DATE_TRUNC('{date_trunc}', created_at)::text as date,
                COUNT(*)::bigint as requests,
                COALESCE(SUM(input_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(output_tokens), 0)::bigint as output_tokens,
                COALESCE(SUM(cache_read_tokens), 0)::bigint as cache_read_tokens,
                COALESCE(SUM(filtered_cost), 0)::bigint as cost_nano
            FROM metric_usage
            GROUP BY DATE_TRUNC('{date_trunc}', created_at)
            ORDER BY date ASC
            "#
    );

    arm(tx, deadline).await?;
    let rows = tx
        .query(&query, &[&org_id, &start, &end, &credit_type])
        .await
        .map_err(map_db_error)?;

    let data: Vec<TimeSeriesPoint> = rows
        .iter()
        .map(|row| TimeSeriesPoint {
            date: row.get(0),
            requests: row.get(1),
            input_tokens: row.get(2),
            output_tokens: row.get(3),
            cache_read_tokens: row.get(4),
            cost_usd: nano_to_usd(row.get::<_, i64>(5)),
        })
        .collect();

    Ok(TimeSeriesMetrics {
        organization_id: org_id,
        organization_name: org_name,
        period_start: start,
        period_end: end,
        granularity: granularity.to_string(),
        data,
    })
}
