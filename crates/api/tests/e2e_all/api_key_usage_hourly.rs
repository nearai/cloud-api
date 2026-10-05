//! The workspace API-key list reads exact lifetime inference usage from `usage_rows`:
//! settled hours from usage_hourly, recent hours from raw.

use crate::admin_provider_attribution_support::{
    isolated_usage_hours, setup_platform_provider_usage_fixture, PlatformProviderUsageFixture,
};
use crate::common::*;
use crate::usage_hourly::{insert_raw, recompute_usage_hours};
use chrono::Duration;

async fn listed_usage(fixture: &PlatformProviderUsageFixture) -> Option<i64> {
    let response = fixture
        .server
        .get(&format!("/v1/workspaces/{}/api-keys", fixture.workspace_id))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response
        .json::<api::models::ListApiKeysResponse>()
        .api_keys
        .into_iter()
        .find(|key| key.id == fixture.api_key_id.to_string())
        .expect("fixture key is listed")
        .usage
        .map(|usage| usage.amount)
}

#[tokio::test]
async fn api_key_list_inference_usage_is_exact_without_waiting_for_the_rollup() {
    let fixture = setup_platform_provider_usage_fixture().await;
    // A row in the re-read window is served from raw immediately, with no recompute.
    insert_raw(
        &fixture,
        chrono::Utc::now() - Duration::minutes(1),
        3_000_000_000,
        1,
        None,
        None,
        Some("external"),
    )
    .await;
    assert_eq!(listed_usage(&fixture).await, Some(3_000_000_000));

    // A row that lands in an already-settled hour (older than the re-read window) is served
    // from usage_hourly, so it counts once that hour is recomputed (the admin repair path).
    // Aggregating the slot's second hour first puts `hour` below the watermark no matter
    // what the shared database already holds.
    let (hour, slot_end) = isolated_usage_hours(&fixture, 2).await;
    let hour_end = hour + Duration::hours(1);
    insert_raw(
        &fixture,
        hour_end + Duration::minutes(5),
        1_000_000_000,
        1,
        None,
        None,
        Some("external"),
    )
    .await;
    recompute_usage_hours(hour_end, slot_end).await;
    assert_eq!(listed_usage(&fixture).await, Some(4_000_000_000));

    insert_raw(
        &fixture,
        hour + Duration::minutes(5),
        5_000_000_000,
        1,
        None,
        None,
        Some("external"),
    )
    .await;
    assert_eq!(listed_usage(&fixture).await, Some(4_000_000_000));
    recompute_usage_hours(hour, hour_end).await;
    assert_eq!(listed_usage(&fixture).await, Some(9_000_000_000));
}
