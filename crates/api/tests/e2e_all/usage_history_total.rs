//! The Usage page's history `total` must not cost a scan of the organization's
//! whole usage log. It comes from the organization's request counter, which the
//! posting transaction increments once per inserted usage row.
use crate::admin_provider_attribution_support::{
    setup_platform_provider_usage_fixture, PlatformProviderUsageFixture,
};
use crate::common::*;
use database::{models::RecordUsageRequest, repositories::OrganizationUsageRepository};
use services::usage::InferenceType;
use uuid::Uuid;

async fn record_usage(fixture: &PlatformProviderUsageFixture, count: usize) {
    let repository = OrganizationUsageRepository::new(fixture.database.pool().clone());
    for _ in 0..count {
        repository
            .record_usage(RecordUsageRequest {
                organization_id: fixture.organization_id,
                workspace_id: fixture.workspace_id,
                api_key_id: fixture.api_key_id,
                model_id: fixture.model_id,
                model_name: fixture.model_name.clone(),
                input_tokens: 1,
                output_tokens: 1,
                input_cost: 1,
                output_cost: 0,
                total_cost: 1,
                inference_type: InferenceType::ChatCompletion.as_str().to_string(),
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id: Some(Uuid::new_v4()),
                provider_request_id: Some(format!("usage-history-total-{}", Uuid::new_v4())),
                stop_reason: None,
                response_id: None,
                image_count: None,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                billing_details: None,
                service_tier: None,
                context_band: None,
                served_provider_tier: None,
                served_provider_type: None,
                served_via_fallback: false,
            })
            .await
            .expect("record usage");
    }
}

async fn history(
    fixture: &PlatformProviderUsageFixture,
    limit: i64,
    offset: i64,
) -> api::routes::usage::UsageHistoryResponse {
    let response = fixture
        .server
        .get(
            format!(
                "/v1/organizations/{}/usage/history?limit={limit}&offset={offset}",
                fixture.organization_id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response.json::<api::routes::usage::UsageHistoryResponse>()
}

#[tokio::test]
async fn usage_history_total_counts_recorded_usage_on_every_page() {
    let fixture = setup_platform_provider_usage_fixture().await;
    record_usage(&fixture, 3).await;

    let first = history(&fixture, 1, 0).await;
    assert_eq!(first.total, 3);
    assert_eq!(first.data.len(), 1);

    let past_end = history(&fixture, 1, 3).await;
    assert_eq!(past_end.total, 3);
    assert!(past_end.data.is_empty());
}

#[tokio::test]
async fn usage_history_total_reads_the_request_counter_not_the_log() {
    // A lifetime COUNT(*) over a large organization's usage log timed the Usage
    // page out at 30s. The counter is kept in the posting transaction, so reading
    // it is one primary-key lookup; a counter that differs from the row count
    // shows which of the two the endpoint reads.
    let fixture = setup_platform_provider_usage_fixture().await;
    record_usage(&fixture, 3).await;
    fixture
        .database
        .pool()
        .get()
        .await
        .expect("db connection")
        .execute(
            "UPDATE organization_balance SET total_requests = 7 WHERE organization_id = $1",
            &[&fixture.organization_id],
        )
        .await
        .expect("set the request counter");

    let page = history(&fixture, 10, 0).await;
    assert_eq!(page.total, 7);
    assert_eq!(page.data.len(), 3);
}
