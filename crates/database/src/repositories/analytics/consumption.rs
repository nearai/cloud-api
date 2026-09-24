//! Per-model consumption and performance timeseries for admin dashboards.

use super::arm;
use super::nano_to_usd;
use crate::repositories::utils::map_db_error;
use services::admin::{
    ModelConsumptionPoint, ModelConsumptionTimeseries, ModelConsumptionTimeseriesQuery,
    PerformancePoint, PerformanceTimeseries, PerformanceTimeseriesQuery,
};
use services::common::RepositoryError;
use std::time::Instant;
use tokio_postgres::Transaction;

pub(super) async fn get_model_consumption_timeseries(
    tx: &Transaction<'_>,
    deadline: Instant,
    query: ModelConsumptionTimeseriesQuery,
) -> Result<ModelConsumptionTimeseries, RepositoryError> {
    // granularity is already an allowlisted &'static str from the handler
    let date_trunc = query.granularity.as_str();

    // Step 1: identify the top-N model_ids by total cost in the period.
    // We use model_id (UUID) as the grouping key to survive model renames.
    arm(tx, deadline).await?;
    let top_ids_rows = tx
        .query(
            r#"
                SELECT model_id
                FROM organization_usage_log
                WHERE created_at >= $1 AND created_at < $2
                GROUP BY model_id
                ORDER BY SUM(total_cost) DESC
                LIMIT $3
                "#,
            &[&query.start, &query.end, &query.top_n],
        )
        .await
        .map_err(map_db_error)?;

    let top_ids: Vec<uuid::Uuid> = top_ids_rows.iter().map(|r| r.get(0)).collect();

    // Step 2: time-bucketed aggregation. Models in top_ids get their current
    // canonical name from models.model_name; all others collapse to "Other".
    let bucket_query = format!(
        r#"
            SELECT
                DATE_TRUNC('{date_trunc}', ul.created_at)::text AS bucket,
                CASE
                    WHEN ul.model_id = ANY($3) THEN COALESCE(m.model_name, ul.model_name)
                    ELSE 'Other'
                END AS model_label,
                COALESCE(SUM(ul.total_cost), 0)::bigint AS cost_nano,
                COUNT(*)::bigint AS requests,
                COALESCE(SUM(ul.total_tokens), 0)::bigint AS tokens
            FROM organization_usage_log ul
            LEFT JOIN models m ON m.id = ul.model_id
            WHERE ul.created_at >= $1 AND ul.created_at < $2
            GROUP BY 1, 2
            ORDER BY 1 ASC, cost_nano DESC
            "#
    );

    arm(tx, deadline).await?;
    let rows = tx
        .query(&bucket_query, &[&query.start, &query.end, &top_ids])
        .await
        .map_err(map_db_error)?;

    // Accumulate total cost per label across all buckets, then sort descending
    // so model_labels reflects true global top-N rank (not first-bucket order).
    let mut label_totals: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let data: Vec<ModelConsumptionPoint> = rows
        .iter()
        .map(|row| {
            let label: String = row.get(1);
            let cost_nano: i64 = row.get(2);
            *label_totals.entry(label.clone()).or_insert(0) += cost_nano;
            ModelConsumptionPoint {
                bucket: row.get(0),
                model_label: label,
                consumed_cost_usd: nano_to_usd(cost_nano),
                requests: row.get(3),
                tokens: row.get(4),
            }
        })
        .collect();

    // Sort: top models by total period cost DESC; "Other" always last.
    let mut model_labels_ordered: Vec<String> = label_totals.keys().cloned().collect();
    model_labels_ordered.sort_by(|a, b| {
        if a == "Other" {
            return std::cmp::Ordering::Greater;
        }
        if b == "Other" {
            return std::cmp::Ordering::Less;
        }
        let ta = label_totals.get(a).copied().unwrap_or(0);
        let tb = label_totals.get(b).copied().unwrap_or(0);
        tb.cmp(&ta)
    });

    Ok(ModelConsumptionTimeseries {
        period_start: query.start,
        period_end: query.end,
        granularity: query.granularity,
        model_labels: model_labels_ordered,
        data,
    })
}

pub(super) async fn get_performance_timeseries(
    tx: &Transaction<'_>,
    deadline: Instant,
    query: PerformanceTimeseriesQuery,
) -> Result<PerformanceTimeseries, RepositoryError> {
    let date_trunc = query.granularity.as_str();

    // Optional model_name filter: $3::text IS NULL OR ul.model_name = $3
    let sql = format!(
        r#"
            SELECT
                DATE_TRUNC('{date_trunc}', ul.created_at)::text AS bucket,
                COUNT(*)::bigint AS requests,
                COALESCE(SUM(ul.total_tokens), 0)::bigint AS total_tokens,
                COALESCE(SUM(ul.output_tokens), 0)::bigint AS output_tokens,
                COUNT(*) FILTER (WHERE ul.ttft_ms IS NOT NULL)::bigint AS ttft_sample_count,
                PERCENTILE_CONT(0.50) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision AS p50_ttft_ms,
                PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision AS p95_ttft_ms,
                PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision AS p99_ttft_ms,
                CASE
                    WHEN COUNT(*) FILTER (WHERE ul.stop_reason IS NOT NULL) = 0 THEN NULL
                    ELSE COUNT(*) FILTER (WHERE ul.stop_reason IN ('provider_error', 'timeout', 'incomplete'))::float8
                         / COUNT(*) FILTER (WHERE ul.stop_reason IS NOT NULL)::float8
                END AS error_rate
            FROM organization_usage_log ul
            WHERE ul.created_at >= $1 AND ul.created_at < $2
              AND ($3::text IS NULL OR ul.model_name = $3)
            GROUP BY 1
            ORDER BY 1 ASC
            "#
    );

    arm(tx, deadline).await?;
    let rows = tx
        .query(&sql, &[&query.start, &query.end, &query.model_name])
        .await
        .map_err(map_db_error)?;

    let data: Vec<PerformancePoint> = rows
        .iter()
        .map(|row| PerformancePoint {
            bucket: row.get(0),
            requests: row.get(1),
            total_tokens: row.get(2),
            output_tokens: row.get(3),
            ttft_sample_count: row.get(4),
            p50_ttft_ms: row.get(5),
            p95_ttft_ms: row.get(6),
            p99_ttft_ms: row.get(7),
            error_rate: row.get(8),
        })
        .collect();

    Ok(PerformanceTimeseries {
        period_start: query.start,
        period_end: query.end,
        granularity: query.granularity,
        model_filter: query.model_name,
        data,
    })
}
