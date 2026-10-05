//! Per-model consumption and performance timeseries for admin dashboards over the exact
//! requested range, read from `usage_rows`.

use super::{approx_percentile, arm, nano_to_usd};
use crate::repositories::usage_hourly::with_usage_rows;
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
    let (start, end) = (query.start, query.end);
    // granularity is already an allowlisted &'static str from the handler
    let date_trunc = query.granularity.as_str();

    // Step 1: identify the top-N model_ids by total cost in the period.
    // We use model_id (UUID) as the grouping key to survive model renames.
    arm(tx, deadline).await?;
    let top_ids_rows = tx
        .query(
            &with_usage_rows(
                "$1",
                "$2",
                r#"
                SELECT model_id
                FROM usage_rows
                GROUP BY model_id
                ORDER BY SUM(total_cost) DESC
                LIMIT $3
                "#,
            ),
            &[&start, &end, &query.top_n],
        )
        .await
        .map_err(map_db_error)?;

    let top_ids: Vec<uuid::Uuid> = top_ids_rows.iter().map(|r| r.get(0)).collect();

    // Step 2: time-bucketed aggregation. Models in top_ids get their current
    // canonical name from models.model_name; all others collapse to "Other".
    let bucket_query = with_usage_rows(
        "$1",
        "$2",
        &format!(
            r#"
            SELECT
                DATE_TRUNC('{date_trunc}', uh.hour)::text AS bucket,
                CASE
                    WHEN uh.model_id = ANY($3) THEN COALESCE(m.model_name, uh.model_name)
                    ELSE 'Other'
                END AS model_label,
                COALESCE(SUM(uh.total_cost), 0)::bigint AS cost_nano,
                COALESCE(SUM(uh.request_count), 0)::bigint AS requests,
                COALESCE(SUM(uh.total_tokens), 0)::bigint AS tokens
            FROM usage_rows uh
            LEFT JOIN models m ON m.id = uh.model_id
            GROUP BY 1, 2
            ORDER BY 1 ASC, cost_nano DESC
            "#
        ),
    );

    arm(tx, deadline).await?;
    let rows = tx
        .query(&bucket_query, &[&start, &end, &top_ids])
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
        period_start: start,
        period_end: end,
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
    let (start, end) = (query.start, query.end);
    let date_trunc = query.granularity.as_str();
    let p50_ttft = approx_percentile("uh.ttft_p50_ms", "uh.ttft_count");
    let p95_ttft = approx_percentile("uh.ttft_p95_ms", "uh.ttft_count");
    let p99_ttft = approx_percentile("uh.ttft_p99_ms", "uh.ttft_count");

    // Optional model_name filter: $3::text IS NULL OR uh.model_name = $3. The error rate
    // counts provider_error, timeout (error_count) and incomplete (incomplete_count) over
    // rows with a recorded stop_reason.
    let sql = with_usage_rows(
        "$1",
        "$2",
        &format!(
            r#"
            SELECT
                DATE_TRUNC('{date_trunc}', uh.hour)::text AS bucket,
                COALESCE(SUM(uh.request_count), 0)::bigint AS requests,
                COALESCE(SUM(uh.total_tokens), 0)::bigint AS total_tokens,
                COALESCE(SUM(uh.output_tokens), 0)::bigint AS output_tokens,
                COALESCE(SUM(uh.ttft_count), 0)::bigint AS ttft_sample_count,
                {p50_ttft} AS p50_ttft_ms,
                {p95_ttft} AS p95_ttft_ms,
                {p99_ttft} AS p99_ttft_ms,
                CASE
                    WHEN COALESCE(SUM(uh.stop_reason_count), 0) = 0 THEN NULL
                    ELSE (SUM(uh.error_count) + SUM(uh.incomplete_count))::float8
                         / SUM(uh.stop_reason_count)::float8
                END AS error_rate
            FROM usage_rows uh
            WHERE ($3::text IS NULL OR uh.model_name = $3)
            GROUP BY 1
            ORDER BY 1 ASC
            "#
        ),
    );

    arm(tx, deadline).await?;
    let rows = tx
        .query(&sql, &[&start, &end, &query.model_name])
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
        period_start: start,
        period_end: end,
        granularity: query.granularity,
        model_filter: query.model_name,
        data,
    })
}
