use crate::admin_provider_attribution_support::{
    insert_platform_provider_usage_row, isolated_provider_usage_window, provider_tier_usage,
    provider_type_usage, setup_platform_provider_usage_fixture, ProviderUsageSeedRow,
};
use crate::common::*;
use crate::usage_hourly::recompute_usage_hours;
use services::admin::{PlatformMetrics, PlatformTimeSeriesMetrics};

const COST_EPSILON: f64 = 1e-9;

#[tokio::test]
async fn admin_platform_metrics_reports_fallback_and_chutes_usage() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let (start, end) = isolated_provider_usage_window(&fixture).await;

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
    recompute_usage_hours(start, end).await;

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
    let metrics: PlatformMetrics =
        serde_json::from_value(response_json).expect("parse PlatformMetrics");

    assert_eq!(metrics.total_requests, 4, "all rows counted once");
    assert_eq!(metrics.total_tokens, 221);
    assert_eq!(metrics.provider_usage.fallback.requests, 2);
    assert_eq!(metrics.provider_usage.fallback.input_tokens, 80);
    assert_eq!(metrics.provider_usage.fallback.output_tokens, 100);
    assert_eq!(metrics.provider_usage.fallback.total_tokens, 180);
    assert_eq!(metrics.provider_usage.fallback.cache_read_tokens, 5);
    assert!((metrics.provider_usage.fallback.consumed_cost_usd - 18.0).abs() < COST_EPSILON);
    assert_eq!(metrics.provider_usage.non_fallback.requests, 2);
    assert_eq!(metrics.provider_usage.non_fallback.total_tokens, 41);
    assert!((metrics.provider_usage.non_fallback.consumed_cost_usd - 4.0).abs() < COST_EPSILON);

    let chutes = provider_type_usage(&metrics, Some("chutes"));
    assert_eq!(chutes.requests, 1);
    assert_eq!(chutes.input_tokens, 50);
    assert_eq!(chutes.output_tokens, 60);
    assert_eq!(chutes.total_tokens, 110);
    assert_eq!(chutes.cache_read_tokens, 3);
    assert!((chutes.consumed_cost_usd - 11.0).abs() < COST_EPSILON);
    assert_eq!(provider_type_usage(&metrics, None).requests, 1);
    assert_eq!(
        provider_tier_usage(&metrics, Some("attested_3p")).requests,
        1
    );
    assert_eq!(provider_tier_usage(&metrics, None).requests, 1);

    // Each list is ordered NULLS FIRST, then ascending, as the separate GROUP BY
    // queries returned it before they became one GROUPING SETS scan.
    let types: Vec<Option<&str>> = metrics
        .provider_usage
        .by_provider_type
        .iter()
        .map(|usage| usage.provider_type.as_deref())
        .collect();
    assert_eq!(
        types,
        [None, Some("chutes"), Some("external"), Some("vllm")]
    );
    let tiers: Vec<Option<&str>> = metrics
        .provider_usage
        .by_provider_tier
        .iter()
        .map(|usage| usage.provider_tier.as_deref())
        .collect();
    assert_eq!(
        tiers,
        [
            None,
            Some("attested_3p"),
            Some("near"),
            Some("non_attested")
        ]
    );
}

#[tokio::test]
async fn admin_platform_metrics_prefers_served_attribution_over_model_metadata() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let (start, end) = isolated_provider_usage_window(&fixture).await;
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
    recompute_usage_hours(start, end).await;

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
    let metrics: PlatformMetrics =
        serde_json::from_value(response_json).expect("parse PlatformMetrics");

    let chutes = provider_type_usage(&metrics, Some("chutes"));
    assert_eq!(chutes.requests, 1);
    assert_eq!(chutes.total_tokens, 36);
    assert!((chutes.consumed_cost_usd - 5.0).abs() < COST_EPSILON);
    assert!(
        metrics
            .provider_usage
            .by_provider_type
            .iter()
            .all(|usage| usage.provider_type.as_deref() != Some("external")),
        "current model provider_type must not drive provider usage"
    );
}

fn url_time(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339().replace('+', "%2B")
}

async fn admin_json<T: serde::de::DeserializeOwned>(
    fixture: &crate::admin_provider_attribution_support::PlatformProviderUsageFixture,
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

#[tokio::test]
async fn admin_platform_reports_serve_whole_hours_from_usage_hourly() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let (start, end) = isolated_provider_usage_window(&fixture).await;
    for (offset, cost, fallback) in [
        (chrono::Duration::seconds(1), 3_000_000_000_i64, false),
        (chrono::Duration::minutes(50), 7_000_000_000, true),
        // Same dimensions as the row above: both collapse into one usage_hourly row with
        // request_count = 2, so request totals must sum request_count, not count rows.
        (chrono::Duration::minutes(55), 1_000_000_000, true),
    ] {
        insert_platform_provider_usage_row(
            &fixture,
            ProviderUsageSeedRow {
                created_at: start + offset,
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                total_cost: cost,
                served_provider_type: Some("vllm"),
                served_provider_tier: Some("near"),
                served_via_fallback: fallback,
            },
        )
        .await;
    }
    let query = format!("start={}&end={}", url_time(start), url_time(end));

    // A settled hour not recomputed yet (the late-row case the repair endpoint covers): empty.
    let before: PlatformMetrics =
        admin_json(&fixture, &format!("/v1/admin/platform/metrics?{query}")).await;
    assert_eq!((before.period_start, before.period_end), (start, end));
    assert_eq!(before.total_requests, 0);

    recompute_usage_hours(start, end).await;

    let metrics: PlatformMetrics =
        admin_json(&fixture, &format!("/v1/admin/platform/metrics?{query}")).await;
    assert_eq!((metrics.period_start, metrics.period_end), (start, end));
    assert_eq!(metrics.total_requests, 3);
    assert_eq!(metrics.total_tokens, 90);
    assert!((metrics.total_consumed_usd - 11.0).abs() < COST_EPSILON);
    assert_eq!(metrics.active_organizations, 1);
    assert_eq!(
        metrics.verifiable_requests, 3,
        "fixture model is verifiable"
    );
    assert_eq!(metrics.provider_usage.fallback.requests, 2);
    assert_eq!(metrics.top_models[0].model_name, fixture.model_name);
    assert_eq!(metrics.top_models[0].requests, 3);
    assert_eq!(
        metrics.top_organizations[0].organization_id,
        fixture.organization_id
    );
    assert_eq!(metrics.top_organizations[0].requests, 3);

    let series: PlatformTimeSeriesMetrics = admin_json(
        &fixture,
        &format!("/v1/admin/platform/metrics/timeseries?{query}&granularity=hour"),
    )
    .await;
    assert_eq!((series.period_start, series.period_end), (start, end));
    assert_eq!(series.data.len(), 1);
    assert_eq!(
        series.data[0].date,
        start.format("%Y-%m-%d %H:%M:%S+00").to_string()
    );
    assert_eq!(series.data[0].requests, 3);
    assert_eq!(series.data[0].tokens, 90);
    assert_eq!(series.data[0].active_organizations, 1);
}
