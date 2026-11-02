//! Exercise the public repository on connection-local tables. The fixture includes
//! the ledger's reversal view, without requiring global schema changes or locks.

use chrono::{TimeZone, Utc};
use database::{repositories::PgAnalyticsRepository, DbPool};
use deadpool_postgres::{Config, Runtime};
use services::{admin::AnalyticsRepository, common::RepositoryError};
use std::time::Duration;
use tokio_postgres::NoTls;
use uuid::Uuid;

const ORG: Uuid = Uuid::from_u128(1);

async fn fixture() -> anyhow::Result<DbPool> {
    let mut config = Config::new();
    config.host = Some(std::env::var("PGHOST").unwrap_or_else(|_| "localhost".into()));
    config.port = Some(
        std::env::var("PGPORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(5432),
    );
    config.dbname = Some(std::env::var("PGDATABASE").unwrap_or_else(|_| "platform_api".into()));
    config.user = Some(std::env::var("PGUSER").unwrap_or_else(|_| "postgres".into()));
    config.password = Some(std::env::var("PGPASSWORD").unwrap_or_else(|_| "postgres".into()));

    let pool = config.create_pool(Some(Runtime::Tokio1), NoTls)?;
    pool.resize(1);
    let client = pool.get().await?;
    client
        .batch_execute(include_str!("support/organization_credit_metrics.sql"))
        .await?;
    drop(client);

    Ok(pool.into())
}

#[tokio::test]
async fn credit_metrics_preserve_live_allocations_and_empty_dimensions() -> anyhow::Result<()> {
    let pool = fixture().await?;
    let repo = PgAnalyticsRepository::new(pool.clone());
    let start = Utc.with_ymd_and_hms(2026, 10, 2, 16, 0, 0).unwrap();
    let end = Utc.with_ymd_and_hms(2026, 11, 2, 12, 2, 0).unwrap();

    let report = repo
        .get_organization_metrics(ORG, start, end, Some("postpay"))
        .await?;

    // Two allocations on the first request must count its tokens only once.
    // The second request's allocation has been fully reversed and is excluded.
    assert_eq!(report.organization_id, ORG);
    assert_eq!(report.organization_name, "Credit metrics");
    assert_eq!((report.period_start, report.period_end), (start, end));
    assert_eq!(report.summary.total_requests, 2);
    assert_eq!(report.summary.total_input_tokens, 20);
    assert_eq!(report.summary.total_output_tokens, 8);
    assert_eq!(report.summary.total_cache_read_tokens, 6);
    assert_eq!(report.summary.total_cost_usd, 8.0);
    assert_eq!(report.summary.unique_api_keys, 2);
    assert_eq!(report.summary.ttft_measured_requests, 1);
    assert_eq!(report.summary.ttft_under_5s_requests, 0);
    assert_eq!(report.summary.ttft_under_10s_requests, 1);
    assert_eq!(report.summary.ttft_under_60s_requests, 1);

    assert_eq!(report.by_workspace.len(), 3);
    assert_eq!(report.by_api_key.len(), 3);
    assert_eq!(report.by_model.len(), 1);
    assert_eq!(report.by_workspace.last().unwrap().requests, 0);
    assert_eq!(report.by_workspace.last().unwrap().cost_usd, 0.0);
    assert_eq!(report.by_api_key.last().unwrap().requests, 0);
    assert_eq!(report.by_api_key.last().unwrap().cost_usd, 0.0);
    assert_eq!(
        report
            .by_workspace
            .iter()
            .map(|row| row.cost_usd)
            .sum::<f64>(),
        8.0
    );
    assert_eq!(
        report
            .by_api_key
            .iter()
            .map(|row| row.cost_usd)
            .sum::<f64>(),
        8.0
    );

    let model = &report.by_model[0];
    assert_eq!(model.model_name, "model-a");
    assert_eq!(model.requests, 2);
    assert_eq!(model.cost_usd, 8.0);
    assert_eq!(model.avg_ttft_ms, Some(5_000.0));
    assert_eq!(model.p95_ttft_ms, Some(5_000.0));
    assert_eq!(model.avg_itl_ms, Some(2.5));
    assert_eq!(model.p95_itl_ms, Some(2.5));
    assert_eq!(model.ttft_under_10s_requests, 1);

    for granularity in ["hour", "day", "week"] {
        let series = repo
            .get_organization_timeseries(ORG, start, end, granularity, Some("postpay"))
            .await?;

        assert_eq!(
            series.data.iter().map(|point| point.requests).sum::<i64>(),
            2
        );
        assert_eq!(
            series.data.iter().map(|point| point.cost_usd).sum::<f64>(),
            8.0
        );
        assert_eq!(
            series
                .data
                .iter()
                .map(|point| point.input_tokens)
                .sum::<i64>(),
            20
        );
    }

    let empty = repo
        .get_organization_metrics(ORG, start, end, Some("staking_farm"))
        .await?;

    assert_eq!(empty.summary.total_requests, 0);
    assert_eq!(empty.summary.total_cost_usd, 0.0);
    assert_eq!(empty.by_workspace.len(), 3);
    assert!(empty.by_workspace.iter().all(|row| row.requests == 0));
    assert_eq!(empty.by_api_key.len(), 3);
    assert!(empty.by_model.is_empty());

    // A reversal after the first report must be visible without a rollup refresh.
    pool.get()
        .await?
        .execute(
            "UPDATE usage_credit_allocation_reversals SET amount = 2000000000 WHERE allocation_id = 1",
            &[],
        )
        .await?;
    let updated = repo
        .get_organization_metrics(ORG, start, end, Some("postpay"))
        .await?;

    assert_eq!(updated.summary.total_requests, 2);
    assert_eq!(updated.summary.total_cost_usd, 7.0);

    pool.get()
        .await?
        .execute(
            "INSERT INTO usage_credit_allocations
             VALUES (11, $1, $2, 'postpay', 2000000000)",
            &[&ORG, &Uuid::from_u128(7)],
        )
        .await?;
    let settled = repo
        .get_organization_metrics(ORG, start, end, Some("postpay"))
        .await?;
    let settled_series = repo
        .get_organization_timeseries(ORG, start, end, "day", Some("postpay"))
        .await?;

    assert_eq!(settled.summary.total_requests, 3);
    assert_eq!(settled.summary.total_cost_usd, 9.0);
    assert_eq!(
        settled_series
            .data
            .iter()
            .map(|point| point.cost_usd)
            .sum::<f64>(),
        9.0
    );

    Ok(())
}

#[tokio::test]
async fn credit_metrics_read_allocations_once_for_all_breakdowns() -> anyhow::Result<()> {
    let pool = fixture().await?;
    let client = pool.get().await?;
    client
        .batch_execute(
            r#"
            CREATE TEMP SEQUENCE allocation_reads;
            CREATE OR REPLACE TEMP VIEW effective_usage_credit_allocations AS
                SELECT a.organization_id, a.inference_usage_id, a.credit_type, a.amount
                FROM usage_credit_allocations a
                WHERE nextval('allocation_reads') > 0;
            "#,
        )
        .await?;
    drop(client);

    let repo = PgAnalyticsRepository::new(pool.clone());
    let start = Utc.with_ymd_and_hms(2026, 10, 2, 16, 0, 0).unwrap();
    let end = Utc.with_ymd_and_hms(2026, 11, 2, 12, 2, 0).unwrap();
    let report = repo
        .get_organization_metrics(ORG, start, end, Some("postpay"))
        .await?;

    assert_eq!(report.summary.total_requests, 3);

    let client = pool.get().await?;
    let reads: i64 = client
        .query_one("SELECT last_value FROM allocation_reads", &[])
        .await?
        .get(0);
    let allocations: i64 = client
        .query_one("SELECT COUNT(*) FROM usage_credit_allocations", &[])
        .await?
        .get(0);

    // The optimizer may evaluate the predicate before or after the range filter,
    // but no allocation should be read again for another report section.
    assert!(
        reads <= allocations,
        "read {reads} allocations from {allocations} rows"
    );

    Ok(())
}

#[tokio::test]
async fn credit_metrics_keep_one_transaction_local_timeout_budget() -> anyhow::Result<()> {
    let pool = fixture().await?;
    let client = pool.get().await?;
    let original_timeout: String = client
        .query_one("SHOW statement_timeout", &[])
        .await?
        .get(0);
    client
        .batch_execute(
            r#"
            ALTER TABLE organizations RENAME TO fixture_organizations;
            CREATE TEMP VIEW organizations AS
                SELECT o.* FROM fixture_organizations o CROSS JOIN pg_sleep(0.25);
            ALTER VIEW effective_usage_credit_allocations RENAME TO fixture_allocations;
            CREATE TEMP VIEW effective_usage_credit_allocations AS
                SELECT a.* FROM fixture_allocations a CROSS JOIN pg_sleep(0.25);
            "#,
        )
        .await?;
    drop(client);

    let repo =
        PgAnalyticsRepository::with_statement_timeout(pool.clone(), Duration::from_millis(400));
    let start = Utc.with_ymd_and_hms(2026, 10, 2, 16, 0, 0).unwrap();
    let end = Utc.with_ymd_and_hms(2026, 11, 2, 12, 2, 0).unwrap();

    let metrics = repo
        .get_organization_metrics(ORG, start, end, Some("postpay"))
        .await;
    let timeseries = repo
        .get_organization_timeseries(ORG, start, end, "day", Some("postpay"))
        .await;

    assert!(matches!(metrics, Err(RepositoryError::QueryTimeout)));
    assert!(matches!(timeseries, Err(RepositoryError::QueryTimeout)));

    let client = pool.get().await?;
    let timeout: String = client
        .query_one("SHOW statement_timeout", &[])
        .await?
        .get(0);
    assert_eq!(timeout, original_timeout);
    drop(client);

    let repo = PgAnalyticsRepository::with_statement_timeout(pool.clone(), Duration::from_secs(5));
    let metrics = repo
        .get_organization_metrics(ORG, start, end, Some("postpay"))
        .await?;

    assert_eq!(metrics.summary.total_cost_usd, 8.0);

    let client = pool.get().await?;
    let timeout: String = client
        .query_one("SHOW statement_timeout", &[])
        .await?
        .get(0);
    assert_eq!(timeout, original_timeout);

    Ok(())
}
