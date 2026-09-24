//! Every analytics report runs inside one statement budget (spec §6.3). Temp views that
//! sleep stand in for slow tables on one pooled connection, so no other test sees them.

use chrono::{Duration, Utc};
use database::repositories::PgAnalyticsRepository;
use services::admin::{
    AnalyticsRepository, ModelConsumptionTimeseriesQuery, ModelRevenueQuery, OrgRevenueQuery,
    PerformanceTimeseriesQuery, RevenueDensityQuery, RevenueSort,
};
use services::common::RepositoryError;
use std::time::Duration as StdDuration;

const BUDGET: StdDuration = StdDuration::from_millis(400);

/// A one-connection pool whose session reads each of `tables` through a temp view that
/// sleeps `seconds` per scan. The sleep is a one-time filter, so it runs before the scan
/// even when no row matches. Returns the pool and the connection's `statement_timeout`.
pub(crate) async fn pool_with_slow_tables(
    tables: &[&str],
    seconds: f64,
) -> (database::pool::DbPool, String) {
    let pool = crate::common::db_setup::create_test_pool().await;
    pool.current().unwrap().resize(1);
    let client = pool.get().await.unwrap();
    let original_timeout: String = client
        .query_one("SHOW statement_timeout", &[])
        .await
        .unwrap()
        .get(0);
    for table in tables {
        client
            .batch_execute(&format!(
                "CREATE TEMP VIEW {table} AS SELECT t.* FROM public.{table} t \
                 WHERE (SELECT true FROM pg_sleep({seconds}))"
            ))
            .await
            .unwrap();
    }
    drop(client);
    (pool, original_timeout)
}

fn assert_timeout<T: std::fmt::Debug>(result: Result<T, RepositoryError>, report: &str) {
    assert!(
        matches!(result, Err(RepositoryError::QueryTimeout)),
        "{report}: {result:?}"
    );
}

#[tokio::test]
async fn every_analytics_report_is_cancelled_at_the_statement_budget() {
    let (pool, original_timeout) = pool_with_slow_tables(
        &[
            "users",
            "organizations",
            "models",
            "organization_limits_history",
            "organization_usage_log",
        ],
        0.5,
    )
    .await;
    let repo = PgAnalyticsRepository::with_statement_timeout(pool.clone(), BUDGET);
    let (start, end) = (Utc::now() - Duration::days(1), Utc::now());
    let org = uuid::Uuid::new_v4(); // the slow `organizations` view cancels before lookup

    assert_timeout(
        repo.get_organization_metrics(org, start, end, None).await,
        "org metrics",
    );
    assert_timeout(
        repo.get_organization_timeseries(org, start, end, "day", None)
            .await,
        "org timeseries",
    );
    assert_timeout(
        repo.get_platform_metrics(start, end).await,
        "platform metrics",
    );
    assert_timeout(
        repo.get_platform_timeseries(start, end, "day").await,
        "platform timeseries",
    );
    assert_timeout(repo.get_billing_summary().await, "billing summary");
    assert_timeout(
        repo.get_model_revenue(ModelRevenueQuery {
            start,
            end,
            verifiable: None,
            provider_type: None,
            model_search: None,
            sort: RevenueSort::Revenue,
            limit: 10,
            offset: 0,
        })
        .await,
        "model revenue",
    );
    assert_timeout(
        repo.get_org_revenue(OrgRevenueQuery {
            start,
            end,
            paying: None,
            search: None,
            sort: RevenueSort::Revenue,
            limit: 10,
            offset: 0,
        })
        .await,
        "org revenue",
    );
    assert_timeout(
        repo.get_model_consumption_timeseries(ModelConsumptionTimeseriesQuery {
            start,
            end,
            granularity: "day".to_string(),
            top_n: 5,
        })
        .await,
        "model consumption",
    );
    assert_timeout(
        repo.get_performance_timeseries(PerformanceTimeseriesQuery {
            start,
            end,
            granularity: "day".to_string(),
            model_name: None,
        })
        .await,
        "performance timeseries",
    );

    let client = pool.get().await.unwrap();
    let timeout: String = client
        .query_one("SHOW statement_timeout", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        timeout, original_timeout,
        "SET LOCAL must not leak after cancellations"
    );
}

/// Review Focus 1: revenue density is an exact reader. It keeps the requested range, to the
/// microsecond, and its own transaction-local budget.
#[tokio::test]
async fn revenue_density_keeps_its_exact_range_in_a_transaction_local_budget() {
    let (pool, original_timeout) = pool_with_slow_tables(&[], 0.0).await;
    let repo = PgAnalyticsRepository::new(pool.clone());
    let end = Utc::now() - Duration::seconds(7);
    let start = end - Duration::minutes(90) + Duration::milliseconds(1_234);

    let report = repo
        .get_revenue_density(RevenueDensityQuery {
            start,
            end,
            provider_type: None,
        })
        .await
        .expect("revenue density");
    assert_eq!((report.period_start, report.period_end), (start, end));

    let client = pool.get().await.unwrap();
    let timeout: String = client
        .query_one("SHOW statement_timeout", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        timeout, original_timeout,
        "SET LOCAL must not leak after success"
    );
}
