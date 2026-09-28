//! Postgres adapter for the usage_hourly aggregate (spec §5.1). Replace semantics from raw
//! organization_usage_log; transaction-scoped advisory lock; UTC hour truncation in SQL.

use crate::pool::DbPool;
use crate::repositories::reporting_query::configure_reporting_transaction;
use crate::repositories::utils::map_db_error;
use anyhow::Context;
use chrono::{DateTime, NaiveDate, Utc};
use services::usage::ports::{
    AggregateLockBehavior, DayParity, DayTotals, HourlyProgress, RecomputeReport,
};
use services::usage::trunc_hour;
use std::time::Duration;
use tokio_postgres::IsolationLevel;

/// Distinct from database_encryption's GLOBAL_WORKER_LOCK_KEY (0x4e454152444245).
pub const USAGE_HOURLY_LOCK_KEY: i64 = 0x55534147454852; // "USAGEHR"
const RECOMPUTE_TIMEOUT: Duration = Duration::from_secs(120);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Catch-up windows read ~390 MB of cold rows on prod; stay single-worker to leave parallel
/// workers for serving traffic.
const SINGLE_WORKER: &str = "SET LOCAL max_parallel_workers_per_gather = 0";

/// usage_hourly's columns in table order; `RAW_HOURLY_SELECT` produces the same shape.
const USAGE_HOURLY_COLUMNS: &str = "hour, organization_id, workspace_id, api_key_id, model_id, \
    model_name, inference_type, served_provider_type, served_provider_tier, served_via_fallback, \
    request_count, input_tokens, output_tokens, cache_read_tokens, total_tokens, total_cost, \
    error_count, incomplete_count, stop_reason_count, \
    ttft_count, ttft_sum_ms, ttft_p50_ms, ttft_p95_ms, ttft_p99_ms, \
    itl_count, itl_sum_ms, itl_p95_ms, last_usage_at";

/// One usage_hourly row per UTC hour and grain from raw rows; callers add FROM, WHERE and
/// `GROUP BY 1..10`. The recompute and `usage_rows_cte` share it, so aggregate hours and
/// raw-served hours can never disagree.
const RAW_HOURLY_SELECT: &str = r#"
SELECT
    date_trunc('hour', created_at, 'UTC'), organization_id, workspace_id, api_key_id, model_id, model_name,
    inference_type, served_provider_type, served_provider_tier, served_via_fallback,
    COUNT(*),
    COALESCE(SUM(input_tokens), 0)::BIGINT, COALESCE(SUM(output_tokens), 0)::BIGINT,
    COALESCE(SUM(cache_read_tokens), 0)::BIGINT, COALESCE(SUM(total_tokens), 0)::BIGINT,
    COALESCE(SUM(total_cost), 0)::BIGINT,
    COUNT(*) FILTER (WHERE stop_reason IN ('provider_error', 'timeout')),
    COUNT(*) FILTER (WHERE stop_reason = 'incomplete'),
    COUNT(stop_reason),
    COUNT(ttft_ms), COALESCE(SUM(ttft_ms), 0)::BIGINT,
    PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY ttft_ms),
    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ttft_ms),
    PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY ttft_ms),
    COUNT(avg_itl_ms), COALESCE(SUM(avg_itl_ms), 0)::DOUBLE PRECISION,
    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY avg_itl_ms),
    MAX(created_at)
"#;

fn recompute_insert() -> String {
    format!(
        "INSERT INTO usage_hourly ({USAGE_HOURLY_COLUMNS}) {RAW_HOURLY_SELECT} \
         FROM organization_usage_log WHERE created_at >= $1 AND created_at < $2 \
         GROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9, 10"
    )
}

/// CTEs `usage_bounds, usage_rows`: rows shaped like usage_hourly that cover exactly
/// `[start, end)` (SQL expressions, usually `$n` parameters). Whole hours below the
/// watermark come from usage_hourly; the partial edge hours and everything at or after the
/// watermark come from raw rows grouped by `RAW_HOURLY_SELECT`. The watermark is the earlier
/// of the last computed hour + 1h and the start of the scheduler's re-read window, so sums,
/// counts and costs equal a raw read of the same range; only cross-hour percentiles are
/// approximate. Raw reads are bounded to the edges plus the re-read tail (an empty aggregate,
/// as during catch-up, reads the whole range raw). Readers select from `usage_rows` without
/// a time predicate of their own.
fn usage_rows_cte(start: &str, end: &str) -> String {
    let reread_hours = services::usage::REREAD_HOURS;
    format!(
        r#"usage_bounds AS MATERIALIZED (
    SELECT agg_from,
           GREATEST(agg_from, LEAST(date_trunc('hour', ({end})::timestamptz, 'UTC'), watermark)) AS agg_to
    FROM (
        SELECT LEAST(
                   CASE WHEN date_trunc('hour', ({start})::timestamptz, 'UTC') = ({start})::timestamptz
                        THEN ({start})::timestamptz
                        ELSE date_trunc('hour', ({start})::timestamptz, 'UTC') + INTERVAL '1 hour'
                   END,
                   ({end})::timestamptz) AS agg_from,
               LEAST(COALESCE((SELECT MAX(hour) FROM usage_hourly) + INTERVAL '1 hour',
                              '-infinity'::timestamptz),
                     date_trunc('hour', now(), 'UTC') - INTERVAL '{reread_hours} hours') AS watermark
    ) bounds
),
usage_rows AS (
    SELECT {USAGE_HOURLY_COLUMNS} FROM usage_hourly
    WHERE hour >= (SELECT agg_from FROM usage_bounds) AND hour < (SELECT agg_to FROM usage_bounds)
    UNION ALL
    {RAW_HOURLY_SELECT}
    FROM (
        SELECT * FROM organization_usage_log
        WHERE created_at >= ({start})::timestamptz AND created_at < (SELECT agg_from FROM usage_bounds)
        UNION ALL
        SELECT * FROM organization_usage_log
        WHERE created_at >= (SELECT agg_to FROM usage_bounds) AND created_at < ({end})::timestamptz
    ) raw
    GROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9, 10
)"#
    )
}

const DAY_TOTALS_RAW: &str = r#"
SELECT COUNT(*)::BIGINT AS request_count,
       COALESCE(SUM(total_tokens), 0)::BIGINT AS total_tokens,
       COALESCE(SUM(total_cost), 0)::BIGINT AS total_cost,
       COUNT(ttft_ms)::BIGINT AS ttft_count,
       COUNT(*) FILTER (WHERE stop_reason IN ('provider_error', 'timeout'))::BIGINT AS error_count,
       COUNT(*) FILTER (WHERE stop_reason = 'incomplete')::BIGINT AS incomplete_count
FROM organization_usage_log WHERE created_at >= $1 AND created_at < $2
"#;

const DAY_TOTALS_AGGREGATE: &str = r#"
SELECT COALESCE(SUM(request_count), 0)::BIGINT AS request_count,
       COALESCE(SUM(total_tokens), 0)::BIGINT AS total_tokens,
       COALESCE(SUM(total_cost), 0)::BIGINT AS total_cost,
       COALESCE(SUM(ttft_count), 0)::BIGINT AS ttft_count,
       COALESCE(SUM(error_count), 0)::BIGINT AS error_count,
       COALESCE(SUM(incomplete_count), 0)::BIGINT AS incomplete_count
FROM usage_hourly WHERE hour >= $1 AND hour < $2
"#;

/// `sql` with the `usage_rows` CTEs over `[start, end)` prepended, merged into `sql`'s own
/// leading `WITH` when it has one.
pub(crate) fn with_usage_rows(start: &str, end: &str, sql: &str) -> String {
    let cte = usage_rows_cte(start, end);
    match sql.trim_start().strip_prefix("WITH ") {
        Some(rest) => format!("WITH {cte},\n{rest}"),
        None => format!("WITH {cte}\n{sql}"),
    }
}

pub struct UsageHourlyRepositoryImpl {
    pool: DbPool,
}

impl UsageHourlyRepositoryImpl {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

fn totals(row: &tokio_postgres::Row) -> DayTotals {
    DayTotals {
        request_count: row.get("request_count"),
        total_tokens: row.get("total_tokens"),
        total_cost: row.get("total_cost"),
        ttft_count: row.get("ttft_count"),
        error_count: row.get("error_count"),
        incomplete_count: row.get("incomplete_count"),
    }
}

#[async_trait::async_trait]
impl services::usage::ports::UsageHourlyRepository for UsageHourlyRepositoryImpl {
    async fn progress(&self) -> anyhow::Result<HourlyProgress> {
        let mut client = self
            .pool
            .get()
            .await
            .context("usage_hourly progress: connection")?;
        let tx = client.transaction().await.map_err(map_db_error)?;
        configure_reporting_transaction(&tx, READ_TIMEOUT).await?;
        let row = tx
            .query_one(
                "WITH m AS (SELECT MAX(hour) AS max_hour FROM usage_hourly)
                 SELECT m.max_hour,
                        (SELECT date_trunc('hour', MIN(created_at), 'UTC') FROM organization_usage_log
                         WHERE created_at >= COALESCE(m.max_hour + INTERVAL '1 hour', '-infinity'::timestamptz))
                 FROM m",
                &[],
            )
            .await
            .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(HourlyProgress {
            max_hour: row.get(0),
            next_raw_hour: row.get(1),
        })
    }

    async fn recompute(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        lock_behavior: AggregateLockBehavior,
    ) -> anyhow::Result<Option<RecomputeReport>> {
        anyhow::ensure!(from < to, "usage_hourly recompute: empty window");
        anyhow::ensure!(
            trunc_hour(from) == from && trunc_hour(to) == to,
            "usage_hourly recompute: bounds must be whole UTC hours"
        );
        let mut client = self
            .pool
            .get()
            .await
            .context("usage_hourly recompute: connection")?;
        let tx = client.transaction().await.map_err(map_db_error)?;
        configure_reporting_transaction(&tx, RECOMPUTE_TIMEOUT).await?;
        tx.batch_execute(SINGLE_WORKER)
            .await
            .map_err(map_db_error)?;
        if lock_behavior == AggregateLockBehavior::Wait {
            tx.execute(
                "SELECT pg_advisory_xact_lock($1)",
                &[&USAGE_HOURLY_LOCK_KEY],
            )
            .await
            .map_err(map_db_error)?;
        } else {
            let locked: bool = tx
                .query_one(
                    "SELECT pg_try_advisory_xact_lock($1)",
                    &[&USAGE_HOURLY_LOCK_KEY],
                )
                .await
                .map_err(map_db_error)?
                .get(0);
            if !locked {
                tx.rollback().await.map_err(map_db_error)?;
                return Ok(None);
            }
        }
        tx.execute(
            "DELETE FROM usage_hourly WHERE hour >= $1 AND hour < $2",
            &[&from, &to],
        )
        .await
        .map_err(map_db_error)?;
        let rows_written = tx
            .execute(&recompute_insert(), &[&from, &to])
            .await
            .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Some(RecomputeReport { rows_written }))
    }

    async fn day_parity(&self, day: NaiveDate) -> anyhow::Result<DayParity> {
        let start = day.and_time(chrono::NaiveTime::MIN).and_utc();
        let end = start + chrono::TimeDelta::days(1);
        let mut client = self
            .pool
            .get()
            .await
            .context("usage_hourly parity: connection")?;
        // One snapshot for both statements: a commit between them must not look like drift.
        let tx = client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await
            .map_err(map_db_error)?;
        configure_reporting_transaction(&tx, READ_TIMEOUT).await?;
        tx.batch_execute(SINGLE_WORKER)
            .await
            .map_err(map_db_error)?;
        let raw = tx
            .query_one(DAY_TOTALS_RAW, &[&start, &end])
            .await
            .map_err(map_db_error)?;
        let aggregate = tx
            .query_one(DAY_TOTALS_AGGREGATE, &[&start, &end])
            .await
            .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(DayParity {
            day,
            raw: totals(&raw),
            aggregate: totals(&aggregate),
        })
    }
}
