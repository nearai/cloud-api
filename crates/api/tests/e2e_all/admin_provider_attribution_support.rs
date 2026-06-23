use crate::common::*;
use services::admin::{ModelRevenueReport, PlatformMetrics};
use std::sync::OnceLock;

static HARNESS_ENV: OnceLock<()> = OnceLock::new();
type ProviderUsageWindow = (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>);

pub(super) struct PlatformProviderUsageFixture {
    pub(super) server: axum_test::TestServer,
    pub(super) database: std::sync::Arc<database::Database>,
    pub(super) organization_id: uuid::Uuid,
    pub(super) workspace_id: uuid::Uuid,
    pub(super) api_key_id: uuid::Uuid,
    pub(super) model_id: uuid::Uuid,
    pub(super) model_name: String,
}

pub(super) struct ProviderUsageSeedRow<'a> {
    pub(super) created_at: chrono::DateTime<chrono::Utc>,
    pub(super) input_tokens: i32,
    pub(super) output_tokens: i32,
    pub(super) cache_read_tokens: i32,
    pub(super) total_cost: i64,
    pub(super) served_provider_type: Option<&'a str>,
    pub(super) served_provider_tier: Option<&'a str>,
    pub(super) served_via_fallback: bool,
}

pub(super) async fn setup_platform_provider_usage_fixture() -> PlatformProviderUsageFixture {
    ensure_platform_provider_usage_harness_env();
    let (server, database) = setup_test_server_with_mock_web_search().await;
    let org = create_org(&server).await;
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces
        .first()
        .expect("organization has default workspace");
    let api_key = create_api_key_in_workspace(
        &server,
        workspace.id.clone(),
        "Provider Usage Metrics Key".to_string(),
    )
    .await;

    let organization_id = uuid::Uuid::parse_str(&org.id).expect("organization id is uuid");
    let workspace_id = uuid::Uuid::parse_str(&workspace.id).expect("workspace id is uuid");
    let api_key_id = uuid::Uuid::parse_str(&api_key.id).expect("api key id is uuid");
    let model_name = format!("test/provider-usage-{}", uuid::Uuid::new_v4());

    let client = database.pool().get().await.expect("db connection");
    let model_id: uuid::Uuid = client
        .query_one(
            r#"
            INSERT INTO models (
                model_name, model_display_name, model_description,
                input_cost_per_token, output_cost_per_token, context_length, max_output_length,
                verifiable, is_active, provider_type, attestation_supported,
                created_at, updated_at
            )
            VALUES ($1, $2, $3, 1, 1, 4096, 1024, true, true, 'external', false, NOW(), NOW())
            RETURNING id
            "#,
            &[&model_name, &model_name, &"Provider usage fixture model"],
        )
        .await
        .expect("insert model")
        .get(0);
    drop(client);

    PlatformProviderUsageFixture {
        server,
        database,
        organization_id,
        workspace_id,
        api_key_id,
        model_id,
        model_name,
    }
}

pub(super) fn ensure_platform_provider_usage_harness_env() {
    HARNESS_ENV.get_or_init(|| {
        std::env::set_var("DEV", "1");
        std::env::set_var("BRAVE_SEARCH_PRO_API_KEY", "test");
    });
}

pub(super) async fn insert_platform_provider_usage_row(
    fixture: &PlatformProviderUsageFixture,
    row: ProviderUsageSeedRow<'_>,
) {
    let client = fixture.database.pool().get().await.expect("db connection");
    let input_cost = row.total_cost / 2;
    let output_cost = row.total_cost - input_cost;
    let total_tokens = row.input_tokens + row.output_tokens;

    client
        .execute(
            r#"
            INSERT INTO organization_usage_log (
                organization_id, workspace_id, api_key_id, model_id, model_name,
                input_tokens, output_tokens, total_tokens, cache_read_tokens,
                input_cost, output_cost, total_cost, request_type, created_at,
                served_provider_tier, served_provider_type, served_via_fallback
            )
            VALUES (
                $1, $2, $3, $4, $5,
                $6, $7, $8, $9,
                $10, $11, $12, 'chat_completion', $13,
                $14, $15, $16
            )
            "#,
            &[
                &fixture.organization_id,
                &fixture.workspace_id,
                &fixture.api_key_id,
                &fixture.model_id,
                &fixture.model_name,
                &row.input_tokens,
                &row.output_tokens,
                &total_tokens,
                &row.cache_read_tokens,
                &input_cost,
                &output_cost,
                &row.total_cost,
                &row.created_at,
                &row.served_provider_tier,
                &row.served_provider_type,
                &row.served_via_fallback,
            ],
        )
        .await
        .expect("insert usage row");
}

pub(super) fn provider_type_usage<'a>(
    metrics: &'a PlatformMetrics,
    provider_type: Option<&str>,
) -> &'a services::admin::ProviderTypeUsage {
    metrics
        .provider_usage
        .by_provider_type
        .iter()
        .find(|usage| usage.provider_type.as_deref() == provider_type)
        .unwrap_or_else(|| panic!("provider type usage not found for: {provider_type:?}"))
}

pub(super) fn provider_tier_usage<'a>(
    metrics: &'a PlatformMetrics,
    provider_tier: Option<&str>,
) -> &'a services::admin::ProviderTierUsage {
    metrics
        .provider_usage
        .by_provider_tier
        .iter()
        .find(|usage| usage.provider_tier.as_deref() == provider_tier)
        .unwrap_or_else(|| panic!("provider tier usage not found for: {provider_tier:?}"))
}

pub(super) fn model_revenue_entry<'a>(
    report: &'a ModelRevenueReport,
    model_name: &str,
) -> &'a services::admin::ModelRevenueEntry {
    report
        .data
        .iter()
        .find(|entry| entry.model_name == model_name)
        .unwrap_or_else(|| panic!("model revenue entry not found for: {model_name}"))
}

pub(super) fn model_provider_breakdown<'a>(
    entry: &'a services::admin::ModelRevenueEntry,
    provider_type: Option<&str>,
    provider_tier: Option<&str>,
    served_via_fallback: bool,
) -> &'a services::admin::ModelProviderRevenueBreakdown {
    entry
        .served_provider_breakdown
        .iter()
        .find(|breakdown| {
            breakdown.provider_type.as_deref() == provider_type
                && breakdown.provider_tier.as_deref() == provider_tier
                && breakdown.served_via_fallback == served_via_fallback
        })
        .unwrap_or_else(|| {
            panic!(
                "model provider breakdown not found for provider_type={provider_type:?}, provider_tier={provider_tier:?}, served_via_fallback={served_via_fallback}"
            )
        })
}

pub(super) fn isolated_provider_usage_window() -> ProviderUsageWindow {
    let offset_minutes = (uuid::Uuid::new_v4().as_u128() % 1_000_000) as i64 * 10;
    let start = chrono::Utc::now()
        + chrono::Duration::days(3650)
        + chrono::Duration::minutes(offset_minutes);
    (start, start + chrono::Duration::minutes(5))
}
