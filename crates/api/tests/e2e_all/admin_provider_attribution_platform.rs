use crate::admin_provider_attribution_support::{
    insert_platform_provider_usage_row, isolated_provider_usage_window, provider_tier_usage,
    provider_type_usage, setup_platform_provider_usage_fixture, ProviderUsageSeedRow,
};
use crate::common::*;
use services::admin::PlatformMetrics;

#[tokio::test]
async fn admin_platform_metrics_reports_fallback_and_chutes_usage() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let (start, end) = isolated_provider_usage_window();

    for row in [
        ProviderUsageSeedRow {
            created_at: start + chrono::Duration::seconds(1),
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 1,
            total_cost: 3_000_000_000,
            served_provider_type: Some("vllm"),
            served_provider_tier: Some("near"),
            served_via_fallback: false,
        },
        ProviderUsageSeedRow {
            created_at: start + chrono::Duration::seconds(2),
            input_tokens: 30,
            output_tokens: 40,
            cache_read_tokens: 2,
            total_cost: 7_000_000_000,
            served_provider_type: Some("external"),
            served_provider_tier: Some("non_attested"),
            served_via_fallback: true,
        },
        ProviderUsageSeedRow {
            created_at: start + chrono::Duration::seconds(3),
            input_tokens: 50,
            output_tokens: 60,
            cache_read_tokens: 3,
            total_cost: 11_000_000_000,
            served_provider_type: Some("chutes"),
            served_provider_tier: Some("attested_3p"),
            served_via_fallback: true,
        },
        ProviderUsageSeedRow {
            created_at: start + chrono::Duration::seconds(4),
            input_tokens: 5,
            output_tokens: 6,
            cache_read_tokens: 4,
            total_cost: 1_000_000_000,
            served_provider_type: None,
            served_provider_tier: None,
            served_via_fallback: false,
        },
    ] {
        insert_platform_provider_usage_row(&fixture, row).await;
    }

    let response = fixture
        .server
        .get(
            format!(
                "/v1/admin/platform/metrics?start={}&end={}",
                start.to_rfc3339().replace('+', "%2B"),
                end.to_rfc3339().replace('+', "%2B")
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200, "platform metrics succeeds");
    let response_json: serde_json::Value =
        serde_json::from_str(&response.text()).expect("response is json");
    println!(
        "provider_usage fallback: {}",
        serde_json::to_string_pretty(&response_json["provider_usage"]["fallback"])
            .expect("fallback json")
    );
    println!(
        "provider_usage by_provider_type: {}",
        serde_json::to_string_pretty(&response_json["provider_usage"]["by_provider_type"])
            .expect("by_provider_type json")
    );
    let metrics: PlatformMetrics =
        serde_json::from_value(response_json).expect("parse PlatformMetrics");

    assert_eq!(metrics.total_requests, 4, "all rows counted once");
    assert_eq!(metrics.total_tokens, 221);
    assert_eq!(metrics.provider_usage.fallback.requests, 2);
    assert_eq!(metrics.provider_usage.fallback.input_tokens, 80);
    assert_eq!(metrics.provider_usage.fallback.output_tokens, 100);
    assert_eq!(metrics.provider_usage.fallback.total_tokens, 180);
    assert_eq!(metrics.provider_usage.fallback.cache_read_tokens, 5);
    assert!((metrics.provider_usage.fallback.consumed_cost_usd - 18.0).abs() < f64::EPSILON);
    assert_eq!(metrics.provider_usage.non_fallback.requests, 2);
    assert_eq!(metrics.provider_usage.non_fallback.total_tokens, 41);
    assert!((metrics.provider_usage.non_fallback.consumed_cost_usd - 4.0).abs() < f64::EPSILON);

    let chutes = provider_type_usage(&metrics, Some("chutes"));
    assert_eq!(chutes.requests, 1);
    assert_eq!(chutes.input_tokens, 50);
    assert_eq!(chutes.output_tokens, 60);
    assert_eq!(chutes.total_tokens, 110);
    assert_eq!(chutes.cache_read_tokens, 3);
    assert!((chutes.consumed_cost_usd - 11.0).abs() < f64::EPSILON);
    assert_eq!(provider_type_usage(&metrics, None).requests, 1);
    assert_eq!(
        provider_tier_usage(&metrics, Some("attested_3p")).requests,
        1
    );
    assert_eq!(provider_tier_usage(&metrics, None).requests, 1);
}

#[tokio::test]
async fn admin_platform_metrics_prefers_served_attribution_over_model_metadata() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let (start, end) = isolated_provider_usage_window();
    insert_platform_provider_usage_row(
        &fixture,
        ProviderUsageSeedRow {
            created_at: start + chrono::Duration::seconds(1),
            input_tokens: 17,
            output_tokens: 19,
            cache_read_tokens: 0,
            total_cost: 5_000_000_000,
            served_provider_type: Some("chutes"),
            served_provider_tier: Some("attested_3p"),
            served_via_fallback: false,
        },
    )
    .await;

    let response = fixture
        .server
        .get(
            format!(
                "/v1/admin/platform/metrics?start={}&end={}",
                start.to_rfc3339().replace('+', "%2B"),
                end.to_rfc3339().replace('+', "%2B")
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200, "platform metrics succeeds");
    let response_json: serde_json::Value =
        serde_json::from_str(&response.text()).expect("response is json");
    println!(
        "provider_usage by_provider_type: {}",
        serde_json::to_string_pretty(&response_json["provider_usage"]["by_provider_type"])
            .expect("by_provider_type json")
    );
    let metrics: PlatformMetrics =
        serde_json::from_value(response_json).expect("parse PlatformMetrics");

    let chutes = provider_type_usage(&metrics, Some("chutes"));
    assert_eq!(chutes.requests, 1);
    assert_eq!(chutes.total_tokens, 36);
    assert!((chutes.consumed_cost_usd - 5.0).abs() < f64::EPSILON);
    assert!(
        metrics
            .provider_usage
            .by_provider_type
            .iter()
            .all(|usage| usage.provider_type.as_deref() != Some("external")),
        "current model provider_type must not drive provider usage"
    );
}
