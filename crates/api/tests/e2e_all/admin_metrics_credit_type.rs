use crate::admin_provider_attribution_support::{
    setup_platform_provider_usage_fixture, PlatformProviderUsageFixture,
};
use crate::common::*;
use chrono::Utc;
use database::repositories::PgAnalyticsRepository;
use services::admin::{AnalyticsRepository, OrganizationMetrics, TimeSeriesMetrics};
use uuid::Uuid;

#[tokio::test]
async fn admin_metrics_credit_type_timeout_is_shared_and_transaction_local() {
    use crate::admin_provider_attribution_support::{
        insert_platform_provider_usage_row, ProviderUsageSeedRow,
    };
    use services::{admin::AnalyticsRepository, common::RepositoryError};
    use std::time::Duration;

    let fixture = setup_platform_provider_usage_fixture().await;
    insert_platform_provider_usage_row(
        &fixture,
        ProviderUsageSeedRow {
            created_at: Utc::now(),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 0,
            total_cost: 1_000_000_000,
            served_provider_type: None,
            served_provider_tier: None,
            served_via_fallback: false,
        },
    )
    .await;
    crate::usage_hourly::recompute_recent_usage().await;
    // Temporary views isolate delays to this test's single pooled connection;
    // no shared-table locks or changes to other tests' data are needed.
    let pool = crate::common::db_setup::create_test_pool().await;
    pool.current().unwrap().resize(1);
    let client = pool.get().await.unwrap();
    let usage_id: Uuid = client
        .query_one(
            "SELECT id FROM organization_usage_log WHERE organization_id = $1",
            &[&fixture.organization_id],
        )
        .await
        .unwrap()
        .get(0);
    let original_timeout: String = client
        .query_one("SHOW statement_timeout", &[])
        .await
        .unwrap()
        .get(0);
    client
        .batch_execute(&format!(
            "CREATE TEMP VIEW organizations AS
         SELECT '{}'::uuid AS id, 'Slow metrics fixture'::text AS name FROM pg_sleep(0.25);
         CREATE TEMP VIEW effective_usage_credit_allocations AS
         SELECT '{}'::uuid AS organization_id, '{}'::uuid AS inference_usage_id,
                'postpay'::text AS credit_type,
                1000000000::bigint AS amount FROM pg_sleep(0.25);",
            fixture.organization_id, fixture.organization_id, usage_id,
        ))
        .await
        .unwrap();
    drop(client);
    let repo = database::repositories::PgAnalyticsRepository::with_statement_timeout(
        pool.clone(),
        Duration::from_millis(400),
    );
    let start = Utc::now() - chrono::Duration::days(1);
    let end = Utc::now() + chrono::Duration::days(1);
    // Each delayed statement is individually shorter than the budget, but the
    // organization lookup and allocation query together exceed it.
    let summary = tokio::time::timeout(
        Duration::from_secs(5),
        repo.get_organization_metrics(fixture.organization_id, start, end, Some("postpay")),
    )
    .await
    .expect("database should cancel before the test deadline");
    assert!(
        matches!(summary, Err(RepositoryError::QueryTimeout)),
        "{summary:?}"
    );
    let timeseries = tokio::time::timeout(
        Duration::from_secs(5),
        repo.get_organization_timeseries(
            fixture.organization_id,
            start,
            end,
            "day",
            Some("postpay"),
        ),
    )
    .await
    .expect("database should cancel before the test deadline");
    assert!(
        matches!(timeseries, Err(RepositoryError::QueryTimeout)),
        "{timeseries:?}"
    );

    let client = pool.get().await.unwrap();
    let timeout: String = client
        .query_one("SHOW statement_timeout", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        timeout, original_timeout,
        "SET LOCAL must not leak after errors"
    );
    drop(client);

    // Successful requests must also restore the pooled connection. The budget now covers
    // unfiltered reports too (spec §6.3), so both run under a budget they fit in.
    let repo = database::repositories::PgAnalyticsRepository::with_statement_timeout(
        pool.clone(),
        Duration::from_secs(5),
    );
    let unfiltered = repo
        .get_organization_metrics(fixture.organization_id, start, end, None)
        .await
        .unwrap();
    assert_eq!(unfiltered.summary.total_requests, 1);
    assert_eq!(unfiltered.summary.total_cost_usd, 1.0);
    let filtered = repo
        .get_organization_timeseries(fixture.organization_id, start, end, "day", Some("postpay"))
        .await
        .unwrap();
    assert_eq!(filtered.data.len(), 1);
    assert_eq!(filtered.data[0].cost_usd, 1.0);
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

fn url_time(t: chrono::DateTime<Utc>) -> String {
    t.to_rfc3339().replace('+', "%2B")
}

async fn org_metrics(
    fixture: &PlatformProviderUsageFixture,
    start: chrono::DateTime<Utc>,
    end: chrono::DateTime<Utc>,
    credit_type: Option<&str>,
) -> OrganizationMetrics {
    PgAnalyticsRepository::new(fixture.database.pool().clone())
        .get_organization_metrics(fixture.organization_id, start, end, credit_type)
        .await
        .expect("organization metrics")
}

async fn org_series(
    fixture: &PlatformProviderUsageFixture,
    start: chrono::DateTime<Utc>,
    end: chrono::DateTime<Utc>,
    granularity: &str,
    credit_type: Option<&str>,
) -> TimeSeriesMetrics {
    PgAnalyticsRepository::new(fixture.database.pool().clone())
        .get_organization_timeseries(
            fixture.organization_id,
            start,
            end,
            granularity,
            credit_type,
        )
        .await
        .expect("organization timeseries")
}

async fn session_json<T: serde::de::DeserializeOwned>(
    server: &axum_test::TestServer,
    path: &str,
) -> T {
    let response = server
        .get(path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response.json()
}

fn assert_close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("value present");
    assert!((actual - expected).abs() < 1e-6, "{actual} != {expected}");
}

#[tokio::test]
async fn org_reports_read_usage_hourly_over_whole_hours() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let org = fixture.organization_id;
    let hour = chrono::Duration::hours(1);
    // Two hours no other test recomputes, so "not aggregated yet" is deterministic.
    let (h, _) = crate::admin_provider_attribution_support::isolated_usage_hours(&fixture, 2).await;
    for (at, cost, ttft) in [
        (h + chrono::Duration::minutes(10), 1_000_000_000_i64, 100),
        (h + chrono::Duration::minutes(20), 1_000_000_000, 200),
        (
            h + hour + chrono::Duration::minutes(10),
            2_000_000_000,
            1000,
        ),
    ] {
        crate::usage_hourly::insert_raw(&fixture, at, cost, 10, Some(ttft), None, Some("external"))
            .await;
    }
    // Whole settled hours are served from usage_hourly.
    let range = format!("start={}&end={}", url_time(h), url_time(h + hour * 2));

    // Settled hours not recomputed yet (the late-row case the repair endpoint covers): empty.
    let before: OrganizationMetrics = org_metrics(&fixture, h, h + hour * 2, None).await;
    assert_eq!((before.period_start, before.period_end), (h, h + hour * 2));
    assert_eq!(before.summary.total_requests, 0);

    crate::usage_hourly::recompute_usage_hours(h, h + hour * 2).await;

    let metrics: OrganizationMetrics = org_metrics(&fixture, h, h + hour * 2, None).await;
    assert_eq!(
        (metrics.period_start, metrics.period_end),
        (h, h + hour * 2)
    );
    assert_eq!(metrics.summary.total_requests, 3);
    assert_eq!(metrics.summary.total_input_tokens, 30);
    assert_eq!(metrics.summary.total_cost_usd, 4.0);
    assert_eq!(metrics.summary.unique_api_keys, 1);
    assert_eq!(
        metrics.by_workspace.iter().map(|w| w.requests).sum::<i64>(),
        3
    );
    assert_eq!(
        metrics.by_api_key.iter().map(|k| k.requests).sum::<i64>(),
        3
    );
    let model = &metrics.by_model[0];
    assert_eq!(model.model_name, fixture.model_name);
    assert_eq!(model.requests, 3);
    // Hour h: p95 of [100, 200] = 195 over 2 samples; hour h+1: 1000 over 1 sample.
    assert_close(model.avg_ttft_ms, 1300.0 / 3.0);
    assert_close(model.p95_ttft_ms, 1390.0 / 3.0);
    // Review Focus 2: no ITL samples at all stays null, not zero.
    assert_eq!((model.avg_itl_ms, model.p95_itl_ms), (None, None));

    let series: TimeSeriesMetrics = org_series(&fixture, h, h + hour * 2, "hour", None).await;
    assert_eq!((series.period_start, series.period_end), (h, h + hour * 2));
    let points: Vec<(String, i64)> = series
        .data
        .iter()
        .map(|p| (p.date.clone(), p.requests))
        .collect();
    assert_eq!(
        points,
        vec![
            (h.format("%Y-%m-%d %H:%M:%S+00").to_string(), 2),
            ((h + hour).format("%Y-%m-%d %H:%M:%S+00").to_string(), 1),
        ]
    );

    // Customer routes read the same bodies and echo the range as RFC 3339.
    let customer: serde_json::Value = session_json(
        &fixture.server,
        &format!("/v1/organizations/{org}/usage/metrics?{range}"),
    )
    .await;
    assert_eq!(customer["period_start"], h.to_rfc3339());
    assert_eq!(customer["period_end"], (h + hour * 2).to_rfc3339());
    assert_eq!(customer["summary"]["total_requests"], 3);
    let customer_series: serde_json::Value = session_json(
        &fixture.server,
        &format!("/v1/organizations/{org}/usage/timeseries?{range}&granularity=hour"),
    )
    .await;
    assert_eq!(customer_series["period_start"], h.to_rfc3339());
    assert_eq!(customer_series["data"].as_array().unwrap().len(), 2);
}

/// TTFT threshold counts: whole hours from usage_hourly, edge hours from raw, and a window
/// equals the sum of its parts.
#[tokio::test]
async fn org_metrics_count_ttft_under_thresholds() {
    use services::admin::{MetricsSummary, ModelMetrics};
    let fixture = setup_platform_provider_usage_fixture().await;
    let hour = chrono::Duration::hours(1);
    let (h, _) = crate::admin_provider_attribution_support::isolated_usage_hours(&fixture, 2).await;
    let minutes = chrono::Duration::minutes;
    for (at, ttft) in [
        (h + minutes(10), Some(4_999)),
        (h + minutes(40), Some(5_000)),
        (h + minutes(50), None),
        (h + hour + minutes(10), Some(9_999)),
        (h + hour + minutes(20), Some(60_000)),
        (h + hour + minutes(30), Some(59_999)),
    ] {
        crate::usage_hourly::insert_raw(&fixture, at, 1, 10, ttft, None, Some("external")).await;
    }
    crate::usage_hourly::recompute_usage_hours(h, h + hour * 2).await;

    fn summary_counts(s: &MetricsSummary) -> [i64; 5] {
        [
            s.total_requests,
            s.ttft_measured_requests,
            s.ttft_under_5s_requests,
            s.ttft_under_10s_requests,
            s.ttft_under_60s_requests,
        ]
    }
    fn model_counts(m: &ModelMetrics) -> [i64; 5] {
        [
            m.requests,
            m.ttft_measured_requests,
            m.ttft_under_5s_requests,
            m.ttft_under_10s_requests,
            m.ttft_under_60s_requests,
        ]
    }
    let get = |from: chrono::DateTime<Utc>, to: chrono::DateTime<Utc>| {
        org_metrics(&fixture, from, to, None)
    };

    // Whole hours: served from usage_hourly.
    let whole = get(h, h + hour * 2).await;
    assert_eq!(summary_counts(&whole.summary), [6, 5, 1, 3, 4]);
    assert_eq!(whole.by_model.len(), 1);
    assert_eq!(model_counts(&whole.by_model[0]), [6, 5, 1, 3, 4]);

    // A window equals the sum of its constituent periods.
    let first = get(h, h + hour).await;
    let second = get(h + hour, h + hour * 2).await;
    let summed: Vec<i64> = summary_counts(&first.summary)
        .iter()
        .zip(summary_counts(&second.summary))
        .map(|(a, b)| a + b)
        .collect();
    assert_eq!(summed, summary_counts(&whole.summary).to_vec());

    // A partial edge hour is read raw and still exact.
    let edge = get(h + minutes(30), h + hour * 2).await;
    assert_eq!(summary_counts(&edge.summary), [5, 4, 0, 2, 3]);
    assert_eq!(model_counts(&edge.by_model[0]), [5, 4, 0, 2, 3]);
}

/// `credit_type` reports read raw; unfiltered ones read usage_rows. Both are exact over the
/// requested range.
#[tokio::test]
async fn org_credit_type_reports_stay_raw_live_and_exact() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let org = fixture.organization_id;
    // An hour no other test recomputes, so "not recomputed yet" is deterministic.
    let (h, _) = crate::admin_provider_attribution_support::isolated_usage_hours(&fixture, 1).await;
    let client = fixture.database.pool().get().await.unwrap();
    let limit_id: Uuid = client
        .query_one(
            "INSERT INTO organization_limits_history (organization_id, credit_type, spend_limit, effective_until)
             VALUES ($1, 'postpay', 0, NOW() - INTERVAL '1 day') RETURNING id",
            &[&org],
        )
        .await
        .unwrap()
        .get(0);
    for (minute, dollars, ttft) in [
        (10_i64, 3_i64, Some(5_000)),
        (20, 1, None),
        (22, 1, Some(10_000)),
        (24, 1, Some(60_000)),
        (50, 4, Some(70_000)),
    ] {
        let at = h + chrono::Duration::minutes(minute);
        crate::usage_hourly::insert_raw(
            &fixture,
            at,
            dollars * 1_000_000_000,
            10,
            ttft,
            None,
            Some("external"),
        )
        .await;
        let usage_id: Uuid = client
            .query_one(
                "SELECT id FROM organization_usage_log WHERE organization_id = $1 AND created_at = $2",
                &[&org, &at],
            )
            .await
            .unwrap()
            .get(0);
        client
            .execute(
                "INSERT INTO usage_credit_allocations
                 (organization_id, inference_usage_id, credit_type, amount, organization_limit_id,
                  policy_version, priority_position, allocation_phase)
                 VALUES ($1, $2, 'postpay', $3, $4, 'test', 0, 'posting')",
                &[&org, &usage_id, &(dollars * 1_000_000_000), &limit_id],
            )
            .await
            .unwrap();
    }
    // Nothing is recomputed. [h, h + 30m) holds four rows: 5000 ms, NULL, 10000 ms and 60000 ms.
    let end = h + chrono::Duration::minutes(30);

    let filtered: OrganizationMetrics = org_metrics(&fixture, h, end, Some("postpay")).await;
    assert_eq!(
        (filtered.period_start, filtered.period_end),
        (h, end),
        "exact range echoed"
    );
    assert_eq!(filtered.summary.total_requests, 4);
    assert_eq!(filtered.summary.total_cost_usd, 6.0);
    // Raw credit_type body: a TTFT exactly at a threshold (5000, 10000, 60000 ms) is not "under" it.
    let s = &filtered.summary;
    assert_eq!(
        [
            s.ttft_measured_requests,
            s.ttft_under_5s_requests,
            s.ttft_under_10s_requests,
            s.ttft_under_60s_requests
        ],
        [3, 0, 1, 2]
    );
    let m = &filtered.by_model[0];
    assert_eq!(m.requests, 4);
    assert_eq!(
        [
            m.ttft_measured_requests,
            m.ttft_under_5s_requests,
            m.ttft_under_10s_requests,
            m.ttft_under_60s_requests
        ],
        [3, 0, 1, 2]
    );

    let series: TimeSeriesMetrics = org_series(&fixture, h, end, "hour", Some("postpay")).await;
    assert_eq!((series.period_start, series.period_end), (h, end));
    assert_eq!(series.data.iter().map(|p| p.requests).sum::<i64>(), 4);

    let unfiltered: OrganizationMetrics = org_metrics(&fixture, h, end, None).await;
    // A sub-hour range reads raw rows, so it is exact without a recompute.
    assert_eq!((unfiltered.period_start, unfiltered.period_end), (h, end));
    assert_eq!(unfiltered.summary.total_requests, 4);
    assert_eq!(unfiltered.summary.total_cost_usd, 6.0);
    let s = &unfiltered.summary;
    assert_eq!(
        [
            s.ttft_measured_requests,
            s.ttft_under_5s_requests,
            s.ttft_under_10s_requests,
            s.ttft_under_60s_requests
        ],
        [3, 0, 1, 2]
    );
}

#[tokio::test]
async fn org_hourly_reports_are_cancelled_at_the_statement_budget() {
    use services::{admin::AnalyticsRepository, common::RepositoryError};
    let fixture = setup_platform_provider_usage_fixture().await;
    let (pool, original_timeout) =
        crate::admin_provider_attribution_support::pool_with_slow_tables(&["usage_hourly"], 0.5)
            .await;
    let repo = database::repositories::PgAnalyticsRepository::with_statement_timeout(
        pool.clone(),
        std::time::Duration::from_millis(400),
    );
    let (start, end) = (Utc::now() - chrono::Duration::days(1), Utc::now());

    let metrics = repo
        .get_organization_metrics(fixture.organization_id, start, end, None)
        .await;
    assert!(
        matches!(metrics, Err(RepositoryError::QueryTimeout)),
        "{metrics:?}"
    );
    let series = repo
        .get_organization_timeseries(fixture.organization_id, start, end, "day", None)
        .await;
    assert!(
        matches!(series, Err(RepositoryError::QueryTimeout)),
        "{series:?}"
    );

    let client = pool.get().await.unwrap();
    let timeout: String = client
        .query_one("SHOW statement_timeout", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(timeout, original_timeout);
}
