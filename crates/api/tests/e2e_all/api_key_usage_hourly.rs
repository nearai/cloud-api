//! The workspace API-key list reads lifetime inference usage from usage_hourly (lagged,
//! spec §6.2) and service usage live from raw, in one statement.

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
async fn api_key_list_inference_usage_reads_usage_hourly() {
    let fixture = setup_platform_provider_usage_fixture().await;
    // An exclusive far-past hour: no other test recomputes it, so "not aggregated yet" holds.
    let (hour, hour_end) = isolated_usage_hours(&fixture, 1).await;
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

    // Not aggregated yet: the documented lag (spec §6.4) shows as no inference usage.
    assert!(matches!(listed_usage(&fixture).await, None | Some(0)));

    recompute_usage_hours(hour, hour_end).await;
    assert_eq!(listed_usage(&fixture).await, Some(5_000_000_000));
}
