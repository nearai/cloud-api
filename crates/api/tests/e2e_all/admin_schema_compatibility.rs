use api::openapi::ApiDoc;
use chrono::{DateTime, Utc};
use services::admin::{
    ModelProviderRevenueBreakdown, ModelRevenueEntry, ModelRevenueReport, PlatformMetrics,
    PlatformProviderUsage, ProviderTypeUsage, ProviderUsageTotals,
};
use utoipa::OpenApi;

fn fixed_admin_timestamp() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-06-22T15:30:00Z")
        .expect("fixed timestamp should parse")
        .with_timezone(&Utc)
}

#[test]
fn admin_provider_attribution_openapi_schema_exports() {
    // Given: the generated OpenAPI document for the API crate.
    let spec = serde_json::to_value(ApiDoc::openapi()).expect("OpenAPI spec should serialize");
    let schemas = spec["components"]["schemas"]
        .as_object()
        .expect("OpenAPI components should include schemas");

    // When: admin platform and model-revenue schemas are exported.
    let platform_metrics = schemas
        .get("PlatformMetrics")
        .expect("PlatformMetrics schema should be exported");
    let model_revenue_entry = schemas
        .get("ModelRevenueEntry")
        .expect("ModelRevenueEntry schema should be exported");

    // Then: provider-attribution schemas are present on admin-only surfaces.
    for schema_name in [
        "PlatformProviderUsage",
        "ProviderUsageTotals",
        "ProviderTypeUsage",
        "ProviderTierUsage",
        "ModelProviderRevenueBreakdown",
    ] {
        assert!(
            schemas.contains_key(schema_name),
            "missing provider attribution schema: {schema_name}"
        );
    }
    assert!(
        platform_metrics["properties"]
            .as_object()
            .expect("PlatformMetrics should expose properties")
            .contains_key("provider_usage"),
        "PlatformMetrics should expose provider_usage"
    );
    let model_revenue_properties = model_revenue_entry["properties"]
        .as_object()
        .expect("ModelRevenueEntry should expose properties");
    for field in [
        "served_provider_breakdown",
        "fallback_requests",
        "fallback_consumed_cost_usd",
    ] {
        assert!(
            model_revenue_properties.contains_key(field),
            "ModelRevenueEntry should expose {field}"
        );
    }

    // Then: customer-facing usage-history schema does not expose raw served-provider fields.
    let usage_history_entry = schemas
        .get("UsageHistoryEntryResponse")
        .expect("UsageHistoryEntryResponse schema should be exported");
    let usage_history_properties = usage_history_entry["properties"]
        .as_object()
        .expect("UsageHistoryEntryResponse should expose properties");
    for forbidden_field in [
        "served_provider_tier",
        "served_provider_type",
        "served_via_fallback",
    ] {
        assert!(
            !usage_history_properties.contains_key(forbidden_field),
            "customer usage history must not expose {forbidden_field}"
        );
    }

    let paths = spec["paths"]
        .as_object()
        .expect("OpenAPI document should include paths");
    assert!(paths.contains_key("/v1/admin/platform/metrics"));
    assert!(paths.contains_key("/v1/admin/platform/model-revenue"));
}

#[test]
fn admin_provider_attribution_preserves_existing_response_fields() {
    // Given: current admin response DTOs with provider attribution populated.
    let now = fixed_admin_timestamp();
    let platform_metrics = PlatformMetrics {
        period_start: now,
        period_end: now,
        generated_at: now,
        total_users: 7,
        total_organizations: 3,
        total_requests: 11,
        total_consumed_usd: 12.5,
        total_tokens: 1200,
        total_cache_read_tokens: 25,
        new_users: 2,
        new_organizations: 1,
        active_organizations: 3,
        paying_organizations: 1,
        verifiable_consumed_usd: 8.0,
        verifiable_requests: 6,
        non_verifiable_consumed_usd: 4.5,
        non_verifiable_requests: 5,
        provider_error_or_timeout_rate: 0.25,
        p95_ttft_ms: Some(123.0),
        provider_usage: PlatformProviderUsage {
            fallback: ProviderUsageTotals {
                requests: 4,
                input_tokens: 100,
                output_tokens: 200,
                total_tokens: 300,
                cache_read_tokens: 10,
                consumed_cost_usd: 1.5,
            },
            non_fallback: ProviderUsageTotals::default(),
            by_provider_type: vec![ProviderTypeUsage {
                provider_type: Some("chutes".to_string()),
                requests: 4,
                input_tokens: 100,
                output_tokens: 200,
                total_tokens: 300,
                cache_read_tokens: 10,
                consumed_cost_usd: 1.5,
            }],
            by_provider_tier: Vec::new(),
        },
        top_models: Vec::new(),
        top_organizations: Vec::new(),
    };
    let model_revenue = ModelRevenueReport {
        period_start: now,
        period_end: now,
        data: vec![ModelRevenueEntry {
            model_name: "qwen/test".to_string(),
            consumed_cost_usd: 2.75,
            requests: 9,
            tokens: 900,
            unique_orgs: 2,
            verifiable: true,
            provider_type: Some("vllm".to_string()),
            avg_ttft_ms: Some(40.0),
            p95_ttft_ms: Some(80.0),
            served_provider_breakdown: vec![ModelProviderRevenueBreakdown {
                provider_type: Some("chutes".to_string()),
                provider_tier: Some("attested_3p".to_string()),
                served_via_fallback: true,
                requests: 3,
                tokens: 300,
                consumed_cost_usd: 0.75,
            }],
            fallback_requests: 3,
            fallback_consumed_cost_usd: 0.75,
        }],
        total: 1,
        limit: 100,
        offset: 0,
    };
    let customer_usage_entry = api::routes::usage::UsageHistoryEntryResponse {
        id: "usage-1".to_string(),
        workspace_id: "workspace-1".to_string(),
        api_key_id: "api-key-1".to_string(),
        model: "qwen/test".to_string(),
        input_tokens: 10,
        output_tokens: 20,
        cache_read_tokens: 0,
        total_tokens: 30,
        total_cost: 1000,
        total_cost_display: "$0.000001".to_string(),
        inference_type: "chat_completion".to_string(),
        created_at: now.to_rfc3339(),
        stop_reason: Some("completed".to_string()),
        response_id: Some("response-1".to_string()),
        provider_request_id: Some("provider-request-1".to_string()),
        inference_id: Some("inference-1".to_string()),
        image_count: None,
    };

    // When: the DTOs are serialized as route JSON responses.
    let platform_json =
        serde_json::to_value(platform_metrics).expect("PlatformMetrics should serialize");
    let model_revenue_json =
        serde_json::to_value(model_revenue).expect("ModelRevenueReport should serialize");
    let usage_json = serde_json::to_value(customer_usage_entry)
        .expect("UsageHistoryEntryResponse should serialize");

    // Then: existing admin response fields remain unchanged and new fields are additive.
    assert_eq!(platform_json["total_requests"], 11);
    assert_eq!(platform_json["total_consumed_usd"], 12.5);
    assert_eq!(platform_json["total_tokens"], 1200);
    assert_eq!(platform_json["provider_usage"]["fallback"]["requests"], 4);
    assert_eq!(model_revenue_json["data"][0]["model_name"], "qwen/test");
    assert_eq!(model_revenue_json["data"][0]["consumed_cost_usd"], 2.75);
    assert_eq!(model_revenue_json["data"][0]["requests"], 9);
    assert_eq!(
        model_revenue_json["data"][0]["served_provider_breakdown"][0]["served_via_fallback"],
        true
    );
    assert_eq!(model_revenue_json["data"][0]["fallback_requests"], 3);

    // Then: customer-facing usage history keeps its old JSON shape and excludes raw attribution.
    assert_eq!(usage_json["model"], "qwen/test");
    assert_eq!(usage_json["total_cost"], 1000);
    let usage_object = usage_json
        .as_object()
        .expect("usage history response should serialize to an object");
    for forbidden_field in [
        "served_provider_tier",
        "served_provider_type",
        "served_via_fallback",
    ] {
        assert!(!usage_object.contains_key(forbidden_field));
    }
}
