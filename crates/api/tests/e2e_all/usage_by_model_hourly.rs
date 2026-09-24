//! GET /v1/organizations/{id}/usage/by-model reads usage_hourly over closed UTC hours,
//! [trunc_hour(now − period), trunc_hour(now)), and echoes the served start (spec §6.1).

use crate::admin_provider_attribution_support::setup_platform_provider_usage_fixture;
use crate::common::*;
use crate::usage_hourly::{insert_raw, recompute_usage_hours};
use chrono::{DateTime, Duration, Utc};
use services::usage::trunc_hour;

#[tokio::test]
async fn usage_by_model_serves_closed_hours_and_echoes_the_hour_start() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let current_hour = trunc_hour(Utc::now());
    let closed = current_hour - Duration::hours(2);
    insert_raw(
        &fixture,
        closed + Duration::minutes(10),
        5_000_000_000,
        7,
        None,
        None,
        Some("external"),
    )
    .await;
    // A row in the next hour stays outside [.., trunc_hour(now)) for at least an hour,
    // even though it is aggregated below.
    insert_raw(
        &fixture,
        current_hour + Duration::minutes(70),
        9_000_000_000,
        3,
        None,
        None,
        Some("external"),
    )
    .await;
    recompute_usage_hours(closed, current_hour + Duration::hours(2)).await;

    let before = Utc::now();
    let response = fixture
        .server
        .get(&format!(
            "/v1/organizations/{}/usage/by-model?period=day",
            fixture.organization_id
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    let after = Utc::now();
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body: serde_json::Value = response.json();

    let start_date = DateTime::parse_from_rfc3339(body["start_date"].as_str().expect("start_date"))
        .expect("RFC 3339")
        .with_timezone(&Utc);
    assert_eq!(
        start_date,
        trunc_hour(start_date),
        "start_date is a whole UTC hour"
    );
    assert!(start_date >= trunc_hour(before - Duration::days(1)));
    assert!(start_date <= trunc_hour(after - Duration::days(1)));

    let data = body["data"].as_array().expect("data");
    assert_eq!(data.len(), 1, "{data:?}");
    assert_eq!(data[0]["model"], fixture.model_name);
    assert_eq!(data[0]["request_count"], 1);
    assert_eq!(data[0]["input_tokens"], 7);
    assert_eq!(data[0]["total_cost"], 5_000_000_000_i64);
}
