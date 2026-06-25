use crate::admin_provider_attribution_support::{
    ensure_platform_provider_usage_harness_env, insert_platform_provider_usage_row,
    model_provider_breakdown, model_revenue_entry, setup_platform_provider_usage_fixture,
    ProviderUsageSeedRow,
};
use crate::common::*;
use services::admin::ModelRevenueReport;

const COST_EPSILON: f64 = 1e-9;

#[tokio::test]
async fn admin_model_revenue_filters_chutes_served_usage() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let client = fixture.database.pool().get().await.expect("db connection");
    client
        .execute(
            "UPDATE models SET provider_type = 'vllm' WHERE id = $1",
            &[&fixture.model_id],
        )
        .await
        .expect("set fixture model provider type");
    drop(client);

    let now = chrono::Utc::now();
    let rows = [
        (
            now - chrono::Duration::minutes(4),
            10,
            15,
            0,
            4_000_000_000,
            Some("vllm"),
            Some("near"),
            false,
        ),
        (
            now - chrono::Duration::minutes(3),
            20,
            25,
            1,
            8_000_000_000,
            Some("external"),
            Some("non_attested"),
            true,
        ),
        (
            now - chrono::Duration::minutes(2),
            30,
            35,
            2,
            16_000_000_000,
            Some("chutes"),
            Some("attested_3p"),
            true,
        ),
        (
            now - chrono::Duration::minutes(1),
            40,
            45,
            3,
            32_000_000_000,
            None,
            None,
            false,
        ),
    ];
    for (
        created_at,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        total_cost,
        provider_type,
        provider_tier,
        fallback,
    ) in rows
    {
        insert_platform_provider_usage_row(
            &fixture,
            ProviderUsageSeedRow {
                created_at,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                total_cost,
                served_provider_type: provider_type,
                served_provider_tier: provider_tier,
                served_via_fallback: fallback,
            },
        )
        .await;
    }

    let start =
        (now - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let end =
        (now + chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let model_search = &fixture.model_name;
    let sid = get_session_id();

    let chutes = fixture
        .server
        .get(
            format!(
                "/v1/admin/platform/model-revenue?start={start}&end={end}&provider_type=chutes&model_search={model_search}"
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {sid}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(chutes.status_code(), 200, "chutes filter should 200");
    let chutes_json: serde_json::Value =
        serde_json::from_str(&chutes.text()).expect("parse chutes response");
    let chutes_report: ModelRevenueReport =
        serde_json::from_value(chutes_json).expect("parse chutes report");
    let chutes_entry = model_revenue_entry(&chutes_report, &fixture.model_name);

    assert_eq!(chutes_entry.provider_type.as_deref(), Some("vllm"));
    assert_eq!(chutes_entry.requests, 1);
    assert_eq!(chutes_entry.tokens, 65);
    assert!((chutes_entry.consumed_cost_usd - 16.0).abs() < COST_EPSILON);
    assert_eq!(chutes_entry.fallback_requests, 1);
    let chutes_breakdown =
        model_provider_breakdown(chutes_entry, Some("chutes"), Some("attested_3p"), true);
    assert_eq!(chutes_breakdown.requests, 1);
    assert_eq!(chutes_breakdown.tokens, 65);
    assert!((chutes_breakdown.consumed_cost_usd - 16.0).abs() < COST_EPSILON);

    let vllm = fixture
        .server
        .get(
            format!(
                "/v1/admin/platform/model-revenue?start={start}&end={end}&provider_type=vllm&model_search={model_search}"
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {sid}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(vllm.status_code(), 200, "vllm filter should 200");
    let vllm_report: ModelRevenueReport =
        serde_json::from_str(&vllm.text()).expect("parse vllm report");
    let vllm_entry = model_revenue_entry(&vllm_report, &fixture.model_name);
    assert_eq!(vllm_entry.requests, 2);
    assert_eq!(vllm_entry.tokens, 110);
    assert_eq!(vllm_entry.fallback_requests, 0);
    assert_eq!(
        model_provider_breakdown(vllm_entry, Some("vllm"), Some("near"), false).requests,
        1
    );
    assert_eq!(
        model_provider_breakdown(vllm_entry, None, None, false).requests,
        1
    );

    let external = fixture
        .server
        .get(
            format!(
                "/v1/admin/platform/model-revenue?start={start}&end={end}&provider_type=external&model_search={model_search}"
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {sid}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(external.status_code(), 200, "external filter should 200");
    let external_report: ModelRevenueReport =
        serde_json::from_str(&external.text()).expect("parse external report");
    let external_entry = model_revenue_entry(&external_report, &fixture.model_name);
    assert_eq!(external_entry.requests, 1);
    assert_eq!(external_entry.tokens, 45);
    assert_eq!(external_entry.fallback_requests, 1);
    assert_eq!(
        model_provider_breakdown(external_entry, Some("external"), Some("non_attested"), true)
            .requests,
        1
    );
}

#[tokio::test]
async fn admin_model_revenue_rejects_invalid_provider_type() {
    ensure_platform_provider_usage_harness_env();
    let (server, _) = setup_test_server_with_mock_web_search().await;

    let response = server
        .get("/v1/admin/platform/model-revenue?provider_type=banana")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "invalid provider_type should 400"
    );
    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "invalid_parameter");
    assert_eq!(
        error.error.message,
        "invalid provider_type 'banana'; expected one of: vllm, external, chutes"
    );
}
