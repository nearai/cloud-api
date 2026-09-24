//! Postgres adapter for the usage_hourly aggregate (spec §5.1). Replace semantics from raw
//! organization_usage_log; transaction-scoped advisory lock; UTC hour truncation in SQL.

use crate::pool::DbPool;
use crate::repositories::reporting_query::configure_reporting_transaction;
use crate::repositories::utils::map_db_error;
use anyhow::Context;
use chrono::{DateTime, NaiveDate, Utc};
use services::usage::ports::{DayParity, DayTotals, HourlyProgress, RecomputeReport};
use services::usage::trunc_hour;
use std::time::Duration;

/// Distinct from database_encryption's GLOBAL_WORKER_LOCK_KEY (0x4e454152444245).
pub const USAGE_HOURLY_LOCK_KEY: i64 = 0x55534147454852; // "USAGEHR"
const RECOMPUTE_TIMEOUT: Duration = Duration::from_secs(120);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

const RECOMPUTE_INSERT: &str = r#"
INSERT INTO usage_hourly (
    hour, organization_id, workspace_id, api_key_id, model_id, model_name,
    inference_type, served_provider_type, served_provider_tier, served_via_fallback,
    request_count, input_tokens, output_tokens, cache_read_tokens, total_tokens, total_cost,
    error_count, incomplete_count, stop_reason_count,
    ttft_count, ttft_sum_ms, ttft_p50_ms, ttft_p95_ms, ttft_p99_ms,
    itl_count, itl_sum_ms, itl_p95_ms, last_usage_at)
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
FROM organization_usage_log
WHERE created_at >= $1 AND created_at < $2
GROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9, 10
"#;

const DAY_TOTALS_RAW: &str = r#"
SELECT COUNT(*)::BIGINT, COALESCE(SUM(total_tokens), 0)::BIGINT, COALESCE(SUM(total_cost), 0)::BIGINT,
       COUNT(ttft_ms)::BIGINT,
       COUNT(*) FILTER (WHERE stop_reason IN ('provider_error', 'timeout'))::BIGINT,
       COUNT(*) FILTER (WHERE stop_reason = 'incomplete')::BIGINT
FROM organization_usage_log WHERE created_at >= $1 AND created_at < $2
"#;

const DAY_TOTALS_AGGREGATE: &str = r#"
SELECT COALESCE(SUM(request_count), 0)::BIGINT, COALESCE(SUM(total_tokens), 0)::BIGINT,
       COALESCE(SUM(total_cost), 0)::BIGINT, COALESCE(SUM(ttft_count), 0)::BIGINT,
       COALESCE(SUM(error_count), 0)::BIGINT, COALESCE(SUM(incomplete_count), 0)::BIGINT
FROM usage_hourly WHERE hour >= $1 AND hour < $2
"#;

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
        request_count: row.get(0),
        total_tokens: row.get(1),
        total_cost: row.get(2),
        ttft_count: row.get(3),
        error_count: row.get(4),
        incomplete_count: row.get(5),
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
        wait: bool,
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
        if wait {
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
            .execute(RECOMPUTE_INSERT, &[&from, &to])
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
        let tx = client.transaction().await.map_err(map_db_error)?;
        configure_reporting_transaction(&tx, READ_TIMEOUT).await?;
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
