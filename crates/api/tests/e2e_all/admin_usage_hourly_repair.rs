//! Admin repair recomputes usage_hourly for an explicit window, picking up raw rows that
//! arrived after their hour left the scheduler's 3-hour re-read.

use crate::admin_provider_attribution_support::{
    insert_platform_provider_usage_row, isolated_provider_usage_window,
    setup_platform_provider_usage_fixture, PlatformProviderUsageFixture, ProviderUsageSeedRow,
};
use crate::common::*;
use crate::usage_hourly::recompute_usage_hours;
use chrono::{DateTime, Duration, Utc};
use services::admin::PlatformMetrics;

fn seed(created_at: DateTime<Utc>) -> ProviderUsageSeedRow<'static> {
    ProviderUsageSeedRow {
        created_at,
        input_tokens: 10,
        output_tokens: 20,
        cache_read_tokens: 0,
        total_cost: 1_000_000_000,
        served_provider_type: Some("vllm"),
        served_provider_tier: Some("near"),
        served_via_fallback: false,
    }
}

async fn platform_requests(
    fixture: &PlatformProviderUsageFixture,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> i64 {
    let response = fixture
        .server
        .get(&format!(
            "/v1/admin/platform/metrics?start={}&end={}",
            start.to_rfc3339().replace('+', "%2B"),
            end.to_rfc3339().replace('+', "%2B")
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response.json::<PlatformMetrics>().total_requests
}

async fn repair(
    fixture: &PlatformProviderUsageFixture,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> axum_test::TestResponse {
    fixture
        .server
        .post("/v1/admin/usage-hourly/recompute")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "start": start, "end": end }))
        .await
}

#[tokio::test]
async fn admin_repair_recomputes_a_window_with_late_raw_rows() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let (start, end) = isolated_provider_usage_window(&fixture).await;
    insert_platform_provider_usage_row(&fixture, seed(start + Duration::seconds(1))).await;
    recompute_usage_hours(start, end).await;
    // A raw row for an already-aggregated hour: the scheduler would never re-read it.
    insert_platform_provider_usage_row(&fixture, seed(start + Duration::seconds(2))).await;
    assert_eq!(platform_requests(&fixture, start, end).await, 1);

    let response = repair(&fixture, start, end).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body: serde_json::Value = response.json();
    assert!(body["rows_written"].as_u64().unwrap() >= 1, "{body}");
    let days = body["days"].as_array().unwrap();
    assert_eq!(days.len(), 1, "{body}");
    assert_eq!(days[0]["day"], start.date_naive().to_string());

    assert_eq!(platform_requests(&fixture, start, end).await, 2);
}

#[tokio::test]
async fn admin_repair_rejects_windows_that_are_not_whole_hours() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let (start, end) = isolated_provider_usage_window(&fixture).await;
    let response = repair(&fixture, start + Duration::minutes(30), end).await;
    assert_eq!(response.status_code(), 400, "{}", response.text());
    let response = repair(&fixture, end, start).await;
    assert_eq!(response.status_code(), 400, "{}", response.text());
}
