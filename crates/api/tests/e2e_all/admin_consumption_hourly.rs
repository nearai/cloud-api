//! Model consumption and performance timeseries read usage_hourly (spec §6.2).

use crate::admin_analytics_statement_budget::pool_with_slow_tables;
use crate::admin_provider_attribution_support::{
    isolated_provider_usage_window, isolated_usage_hours, setup_platform_provider_usage_fixture,
    PlatformProviderUsageFixture,
};
use crate::common::*;
use crate::usage_hourly::{insert_raw, recompute_usage_hours};
use chrono::{Duration, Utc};
use database::repositories::PgAnalyticsRepository;
use services::admin::{
    AnalyticsRepository, ModelConsumptionTimeseries, PerformanceTimeseries,
    PerformanceTimeseriesQuery,
};
use services::common::RepositoryError;

fn url_time(t: chrono::DateTime<Utc>) -> String {
    t.to_rfc3339().replace('+', "%2B")
}

async fn admin_json<T: serde::de::DeserializeOwned>(
    fixture: &PlatformProviderUsageFixture,
    path: &str,
) -> T {
    let response = fixture
        .server
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
async fn model_consumption_ranks_the_widened_hour_and_follows_renames() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let other = setup_platform_provider_usage_fixture().await;
    let (hour, end) = isolated_provider_usage_window(&fixture).await;
    insert_raw(
        &fixture,
        hour + Duration::minutes(1),
        3_000_000_000,
        10,
        None,
        None,
        Some("external"),
    )
    .await;
    insert_raw(
        &fixture,
        hour + Duration::minutes(40),
        3_000_000_000,
        10,
        None,
        None,
        Some("external"),
    )
    .await;
    insert_raw(
        &other,
        hour + Duration::minutes(20),
        1_000_000_000,
        10,
        None,
        None,
        Some("external"),
    )
    .await;
    recompute_usage_hours(hour, end).await;
    let path = format!(
        "/v1/admin/platform/model-consumption-timeseries?start={}&end={}&granularity=hour&top_n=1",
        url_time(hour + Duration::minutes(5)),
        url_time(hour + Duration::minutes(10))
    );

    let report: ModelConsumptionTimeseries = admin_json(&fixture, &path).await;
    assert_eq!((report.period_start, report.period_end), (hour, end));
    assert_eq!(
        report.model_labels,
        vec![fixture.model_name.clone(), "Other".to_string()]
    );
    let bucket = hour.format("%Y-%m-%d %H:%M:%S+00").to_string();
    let top = report
        .data
        .iter()
        .find(|p| p.model_label == fixture.model_name)
        .expect("top model");
    assert_eq!(
        (top.bucket.as_str(), top.requests, top.tokens),
        (bucket.as_str(), 2, 20)
    );
    assert_eq!(top.consumed_cost_usd, 6.0);
    let rest = report
        .data
        .iter()
        .find(|p| p.model_label == "Other")
        .expect("Other");
    assert_eq!((rest.requests, rest.consumed_cost_usd), (1, 1.0));

    // Review Focus 5: labels come from the current models row (joined on model_id), not
    // the name usage_hourly recorded.
    let renamed = format!("{}-renamed", fixture.model_name);
    let client = fixture.database.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE models SET model_name = $2 WHERE id = $1",
            &[&fixture.model_id, &renamed],
        )
        .await
        .unwrap();
    drop(client);
    let report: ModelConsumptionTimeseries = admin_json(&fixture, &path).await;
    assert_eq!(report.model_labels, vec![renamed, "Other".to_string()]);
}

#[tokio::test]
async fn performance_combines_hourly_percentiles_by_sample_count() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let (h, end) = isolated_usage_hours(&fixture, 2).await;
    insert_raw(
        &fixture,
        h + Duration::minutes(10),
        1,
        1,
        Some(100),
        Some("timeout"),
        Some("external"),
    )
    .await;
    insert_raw(
        &fixture,
        h + Duration::minutes(20),
        1,
        1,
        Some(200),
        Some("completed"),
        Some("external"),
    )
    .await;
    insert_raw(
        &fixture,
        h + Duration::minutes(70),
        1,
        1,
        Some(1000),
        Some("incomplete"),
        Some("external"),
    )
    .await;
    // Review Focus 2: its own usage_hourly row with ttft_count = 0 and NULL percentiles.
    insert_raw(
        &fixture,
        h + Duration::minutes(80),
        1,
        1,
        None,
        None,
        Some("chutes"),
    )
    .await;
    recompute_usage_hours(h, end).await;

    let report: PerformanceTimeseries = admin_json(
        &fixture,
        &format!(
            "/v1/admin/platform/performance-timeseries?start={}&end={}&granularity=day&model_name={}",
            url_time(h + Duration::minutes(5)),
            url_time(h + Duration::minutes(90)),
            fixture.model_name
        ),
    )
    .await;
    assert_eq!((report.period_start, report.period_end), (h, end));
    assert_eq!(report.data.len(), 1);
    let point = &report.data[0];
    assert_eq!((point.requests, point.ttft_sample_count), (4, 3));
    // Hour h: [100, 200] → p50 150, p95 195, p99 199 (2 samples); hour h+1: 1000 (1 sample).
    assert_close(point.p50_ttft_ms, 1300.0 / 3.0);
    assert_close(point.p95_ttft_ms, 1390.0 / 3.0);
    assert_close(point.p99_ttft_ms, 1398.0 / 3.0);
    // timeout + incomplete over three recorded stop reasons.
    assert_close(point.error_rate, 2.0 / 3.0);
}

#[tokio::test]
async fn performance_is_cancelled_at_the_statement_budget() {
    let (pool, _) = pool_with_slow_tables(&["usage_hourly"], 0.5).await;
    let repo =
        PgAnalyticsRepository::with_statement_timeout(pool, std::time::Duration::from_millis(400));
    let result = repo
        .get_performance_timeseries(PerformanceTimeseriesQuery {
            start: Utc::now() - Duration::days(1),
            end: Utc::now(),
            granularity: "day".to_string(),
            model_name: None,
        })
        .await;
    assert!(
        matches!(result, Err(RepositoryError::QueryTimeout)),
        "{result:?}"
    );
}
