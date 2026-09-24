//! Organization metrics and timeseries (admin and customer routes) over the exact requested
//! range. Without `credit_type` they read `usage_rows`; with it, raw `organization_usage_log`.

use super::{approx_percentile, arm, nano_to_usd, weighted_mean};
use crate::repositories::usage_hourly::with_usage_rows;
use crate::repositories::utils::map_db_error;
use chrono::{DateTime, Utc};
use services::admin::{
    ApiKeyMetrics, MetricsSummary, ModelMetrics, OrganizationMetrics, TimeSeriesMetrics,
    TimeSeriesPoint, WorkspaceMetrics,
};
use services::common::RepositoryError;
use std::time::Instant;
use tokio_postgres::types::ToSql;
use tokio_postgres::Transaction;
use uuid::Uuid;

// Keep one row per inference request, even when posting and settlement both
// allocate the requested credit type.
const CREDIT_TYPE_USAGE_CTE: &str = r#"
    WITH metric_usage AS (
        SELECT ul.*, allocation.amount AS filtered_cost
        FROM organization_usage_log ul
        LEFT JOIN LATERAL (
            SELECT SUM(a.amount)::BIGINT AS amount
            FROM usage_credit_allocations a
            WHERE a.inference_usage_id = ul.id AND a.credit_type = $4
        ) allocation ON true
        WHERE ul.organization_id = $1 AND ul.created_at >= $2 AND ul.created_at < $3
          AND allocation.amount > 0
    )
"#;

/// The four statements of an organization metrics report. Both sources return the same
/// columns in the same order, so one mapping serves both.
struct MetricsSql {
    summary: String,
    by_workspace: String,
    by_api_key: String,
    by_model: String,
}

/// `usage_rows` for organization `$1` over the exact range `[$2, $3)`.
fn hourly_metrics_sql() -> MetricsSql {
    let avg_ttft = weighted_mean("uh.ttft_sum_ms", "uh.ttft_count");
    let p95_ttft = approx_percentile("uh.ttft_p95_ms", "uh.ttft_count");
    let avg_itl = weighted_mean("uh.itl_sum_ms", "uh.itl_count");
    let p95_itl = approx_percentile("uh.itl_p95_ms", "uh.itl_count");
    MetricsSql {
        summary: with_usage_rows(
            "$2",
            "$3",
            r#"
            SELECT
                COALESCE(SUM(request_count), 0)::bigint as requests,
                COALESCE(SUM(input_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(output_tokens), 0)::bigint as output_tokens,
                COALESCE(SUM(cache_read_tokens), 0)::bigint as cache_read_tokens,
                COALESCE(SUM(total_cost), 0)::bigint as cost_nano,
                COUNT(DISTINCT api_key_id)::bigint as unique_api_keys
            FROM usage_rows
            WHERE organization_id = $1
            "#,
        ),
        by_workspace: with_usage_rows(
            "$2",
            "$3",
            r#"
            SELECT
                w.id as workspace_id,
                w.name as workspace_name,
                COALESCE(SUM(uh.request_count), 0)::bigint as requests,
                COALESCE(SUM(uh.input_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(uh.output_tokens), 0)::bigint as output_tokens,
                COALESCE(SUM(uh.cache_read_tokens), 0)::bigint as cache_read_tokens,
                COALESCE(SUM(uh.total_cost), 0)::bigint as cost_nano
            FROM workspaces w
            LEFT JOIN usage_rows uh ON uh.workspace_id = w.id
                AND uh.organization_id = $1
            WHERE w.organization_id = $1
            GROUP BY w.id, w.name
            ORDER BY requests DESC
            "#,
        ),
        by_api_key: with_usage_rows(
            "$2",
            "$3",
            r#"
            SELECT
                ak.id as api_key_id,
                ak.name as api_key_name,
                COALESCE(SUM(uh.request_count), 0)::bigint as requests,
                COALESCE(SUM(uh.total_cost), 0)::bigint as cost_nano
            FROM api_keys ak
            LEFT JOIN usage_rows uh ON uh.api_key_id = ak.id
                AND uh.organization_id = $1
            WHERE ak.workspace_id IN (
                SELECT id FROM workspaces WHERE organization_id = $1
            )
            GROUP BY ak.id, ak.name
            ORDER BY requests DESC
            "#,
        ),
        by_model: with_usage_rows(
            "$2",
            "$3",
            &format!(
                r#"
            SELECT
                uh.model_name,
                COALESCE(SUM(uh.request_count), 0)::bigint as requests,
                COALESCE(SUM(uh.input_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(uh.output_tokens), 0)::bigint as output_tokens,
                COALESCE(SUM(uh.cache_read_tokens), 0)::bigint as cache_read_tokens,
                COALESCE(SUM(uh.total_cost), 0)::bigint as cost_nano,
                {avg_ttft} as avg_ttft_ms,
                {p95_ttft} as p95_ttft_ms,
                {avg_itl} as avg_itl_ms,
                {p95_itl} as p95_itl_ms
            FROM usage_rows uh
            WHERE uh.organization_id = $1
            GROUP BY uh.model_name
            ORDER BY requests DESC
            "#
            ),
        ),
    }
}

// ponytail: credit_type filters read raw organization_usage_log because allocations settle after
// posting (settle_unfunded_usage), so usage_hourly cannot carry them. Ceiling: a filtered window
// must finish within this repository's statement timeout as a per-org raw scan. Upgrade path: an
// allocation-aware aggregate recomputed after settlement, once settlement has a completion signal.
/// Raw usage with saved credit-type allocations for organization `$1` over the exact
/// `[$2, $3)`, credit type `$4`.
fn credit_type_metrics_sql() -> MetricsSql {
    let cte = CREDIT_TYPE_USAGE_CTE;
    MetricsSql {
        summary: format!(
            r#"{cte}
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
        by_workspace: format!(
            r#"{cte}
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
        by_api_key: format!(
            r#"{cte}
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
        by_model: format!(
            r#"{cte}
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
            "#
        ),
    }
}

/// Bind parameters: `$1..$3` are the organization and range; `$4` (raw bodies only) is the
/// credit type.
fn bind<'a>(
    org_id: &'a Uuid,
    start: &'a DateTime<Utc>,
    end: &'a DateTime<Utc>,
    credit_type: &'a Option<&str>,
) -> Vec<&'a (dyn ToSql + Sync)> {
    let mut params: Vec<&'a (dyn ToSql + Sync)> = vec![org_id, start, end];
    if let Some(credit_type) = credit_type {
        params.push(credit_type);
    }
    params
}

async fn organization_name(
    tx: &Transaction<'_>,
    deadline: Instant,
    org_id: Uuid,
) -> Result<String, RepositoryError> {
    arm(tx, deadline).await?;
    let row = tx
        .query_opt("SELECT name FROM organizations WHERE id = $1", &[&org_id])
        .await
        .map_err(map_db_error)?
        .ok_or_else(|| RepositoryError::NotFound(format!("Organization {org_id}")))?;
    Ok(row.get(0))
}

pub(super) async fn get_organization_metrics_with_client(
    tx: &Transaction<'_>,
    deadline: Instant,
    window: (Uuid, DateTime<Utc>, DateTime<Utc>),
    credit_type: Option<&str>,
) -> Result<OrganizationMetrics, RepositoryError> {
    let (org_id, start, end) = window;
    let sql = match credit_type {
        None => hourly_metrics_sql(),
        // Raw credit_type body: see the ponytail on credit_type_metrics_sql.
        Some(_) => credit_type_metrics_sql(),
    };
    let params = bind(&org_id, &start, &end, &credit_type);
    let org_name = organization_name(tx, deadline, org_id).await?;

    arm(tx, deadline).await?;
    let summary_row = tx
        .query_one(&sql.summary, &params)
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

    arm(tx, deadline).await?;
    let workspace_rows = tx
        .query(&sql.by_workspace, &params)
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

    arm(tx, deadline).await?;
    let api_key_rows = tx
        .query(&sql.by_api_key, &params)
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

    arm(tx, deadline).await?;
    let model_rows = tx
        .query(&sql.by_model, &params)
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
    // Determine date truncation based on granularity
    let date_trunc = match granularity {
        "hour" => "hour",
        "week" => "week",
        _ => "day", // default to day
    };
    let query = match credit_type {
        None => with_usage_rows(
            "$2",
            "$3",
            &format!(
                r#"
            SELECT
                DATE_TRUNC('{date_trunc}', hour)::text as date,
                COALESCE(SUM(request_count), 0)::bigint as requests,
                COALESCE(SUM(input_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(output_tokens), 0)::bigint as output_tokens,
                COALESCE(SUM(cache_read_tokens), 0)::bigint as cache_read_tokens,
                COALESCE(SUM(total_cost), 0)::bigint as cost_nano
            FROM usage_rows
            WHERE organization_id = $1
            GROUP BY DATE_TRUNC('{date_trunc}', hour)
            ORDER BY date ASC
            "#
            ),
        ),
        // ponytail: credit_type filters read raw organization_usage_log because allocations settle after
        // posting (settle_unfunded_usage), so usage_hourly cannot carry them. Ceiling: a filtered window
        // must finish within this repository's statement timeout as a per-org raw scan. Upgrade path: an
        // allocation-aware aggregate recomputed after settlement, once settlement has a completion signal.
        Some(_) => {
            let cte = CREDIT_TYPE_USAGE_CTE;
            format!(
                r#"{cte}
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
            )
        }
    };
    let params = bind(&org_id, &start, &end, &credit_type);
    let org_name = organization_name(tx, deadline, org_id).await?;

    arm(tx, deadline).await?;
    let rows = tx.query(&query, &params).await.map_err(map_db_error)?;
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
