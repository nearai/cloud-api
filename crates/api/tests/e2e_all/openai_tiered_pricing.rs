//! Database-backed coverage for exact OpenAI text pricing dimensions.

use crate::common::*;
use inference_providers::mock::{MockProvider, ResponseTemplate};
use inference_providers::{ChatServiceTier, InferenceProvider, StreamChunk};
use serde_json::{json, Value};
use services::usage::{compute_profiled_text_cost, TextPricingProfile, TextServiceTier};
use std::sync::{Arc, OnceLock};

const CANONICAL_MODEL: &str = "openai/gpt-5.6-sol";
const MODEL_ALIAS: &str = "openai/gpt-5.6";
const STREAMING_MODEL: &str = "openai/gpt-5.6-terra";

static DEV_ENV: OnceLock<()> = OnceLock::new();

fn ensure_env() {
    DEV_ENV.get_or_init(|| {
        std::env::set_var("DEV", "1");
        std::env::set_var("BRAVE_SEARCH_PRO_API_KEY", "openai-tiered-pricing-test");
    });
}

fn profile_json() -> Value {
    json!({
        "version": 1,
        "currency": "USD",
        "unit": "million_tokens",
        "longContextThreshold": 272000,
        "tiers": {
            "default": {
                "short": {"uncachedInput": "5.00", "cachedInput": "0.50", "cacheWrite": "6.25", "output": "30.00"},
                "long": {"uncachedInput": "10.00", "cachedInput": "1.00", "cacheWrite": "12.50", "output": "45.00"}
            },
            "flex": {
                "short": {"uncachedInput": "2.50", "cachedInput": "0.25", "cacheWrite": "3.125", "output": "15.00"},
                "long": {"uncachedInput": "5.00", "cachedInput": "0.50", "cacheWrite": "6.25", "output": "22.50"}
            },
            "priority": {
                "short": {"uncachedInput": "10.00", "cachedInput": "1.00", "cacheWrite": "12.50", "output": "60.00"},
                "long": {"uncachedInput": "20.00", "cachedInput": "2.00", "cacheWrite": "25.00", "output": "90.00"}
            }
        }
    })
}

async fn setup_profiled_model(
    server: &axum_test::TestServer,
    pool: &Arc<services::inference_provider_pool::InferenceProviderPool>,
    provider: Arc<MockProvider>,
    model: &str,
    aliases: &[&str],
) {
    let provider_trait: Arc<dyn InferenceProvider + Send + Sync> = provider;
    pool.register_provider(model.to_string(), provider_trait)
        .await;

    let mut batch = api::models::BatchUpdateModelApiRequest::new();
    batch.insert(
        model.to_string(),
        serde_json::from_value(json!({
            "textPricing": profile_json(),
            "modelDisplayName": "GPT-5.6 Sol",
            "modelDescription": "Tiered pricing integration fixture",
            "contextLength": 1050000,
            "maxOutputLength": 128000,
            "isActive": true,
            "aliases": aliases,
            "ownedBy": "openai",
            "attestationSupported": false,
            "inputModalities": ["text", "image"],
            "outputModalities": ["text"]
        }))
        .unwrap(),
    );
    let models = admin_batch_upsert_models(server, batch, get_session_id()).await;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].input_cost_per_token.amount, 5_000);
    assert_eq!(
        models[0]
            .cache_read_cost_per_token
            .as_ref()
            .expect("cached projection")
            .amount,
        500
    );
    assert_eq!(models[0].output_cost_per_token.amount, 30_000);
    assert_eq!(models[0].text_pricing.as_ref(), Some(&profile_json()));
}

async fn latest_usage(
    server: &axum_test::TestServer,
    organization_id: &str,
) -> api::routes::usage::UsageHistoryEntryResponse {
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    let response = server
        .get(&format!(
            "/v1/organizations/{organization_id}/usage/history?limit=1&offset=0"
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let history: api::routes::usage::UsageHistoryResponse = response.json();
    history.data.into_iter().next().expect("usage entry")
}

#[tokio::test]
async fn fast_alias_downgrade_bills_canonical_default_profile_exactly() {
    ensure_env();
    let (server, pool, _, _) = setup_test_server_with_pool().await;
    let provider = Arc::new(MockProvider::new_accept_all());
    provider
        .set_default_response(
            ResponseTemplate::new("1. 2. 3.")
                .with_cache_tokens(1)
                .with_cache_write_tokens(2)
                .with_service_tier("default"),
        )
        .await;
    setup_profiled_model(
        &server,
        &pool,
        provider.clone(),
        CANONICAL_MODEL,
        &[MODEL_ALIAS],
    )
    .await;

    let organization = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, organization.id.clone()).await;

    let catalog = list_models(&server, api_key.clone()).await;
    let catalog_model = catalog
        .data
        .iter()
        .find(|model| model.id == CANONICAL_MODEL)
        .expect("profiled model in public catalog");
    assert_eq!(catalog_model.text_pricing.as_ref(), Some(&profile_json()));
    assert_eq!(catalog_model.pricing.as_ref().unwrap().input, 5.0);

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": MODEL_ALIAS,
            "messages": [{"role": "user", "content": "hello"}],
            "service_tier": "fast"
        }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let completion: Value = response.json();
    assert_eq!(completion["service_tier"], "default");

    let forwarded = provider
        .last_chat_params()
        .await
        .expect("request reached mock provider");
    assert_eq!(forwarded.model, CANONICAL_MODEL);
    assert_eq!(forwarded.service_tier, Some(ChatServiceTier::Priority));

    let usage = latest_usage(&server, &organization.id).await;
    assert_eq!(usage.model, CANONICAL_MODEL);
    assert_eq!(usage.cache_read_tokens, 1);
    assert_eq!(usage.cache_write_tokens, 2);
    assert_eq!(usage.service_tier.as_deref(), Some("default"));
    assert_eq!(usage.context_band.as_deref(), Some("short"));

    let profile = TextPricingProfile::from_json(profile_json()).unwrap();
    let expected = compute_profiled_text_cost(
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
        TextServiceTier::Priority,
        Some("default"),
        &profile,
    )
    .unwrap();
    assert_eq!(usage.total_cost, expected.cost.total_cost);
    let snapshot = usage.billing_details.expect("billing snapshot");
    assert_eq!(snapshot["requestedTier"], "priority");
    assert_eq!(snapshot["actualTier"], "default");
    assert_eq!(snapshot["pricedTier"], "default");
    assert_eq!(snapshot["rounding"]["roundedTotal"], usage.total_cost);
}

#[tokio::test]
async fn streaming_flex_captures_actual_tier_and_cache_write_usage() {
    ensure_env();
    let (server, pool, _, _) = setup_test_server_with_pool().await;
    let provider = Arc::new(MockProvider::new_accept_all());
    provider
        .set_default_response(
            ResponseTemplate::new("1. 2. 3.")
                .with_cache_write_tokens(2)
                .with_service_tier("flex"),
        )
        .await;
    setup_profiled_model(&server, &pool, provider, STREAMING_MODEL, &[]).await;

    let organization = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, organization.id.clone()).await;
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": STREAMING_MODEL,
            "messages": [{"role": "user", "content": "hello"}],
            "service_tier": "flex",
            "stream": true,
            "stream_options": {"include_usage": true}
        }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let mut saw_flex = false;
    let mut saw_cache_write = false;
    for line in response.text().lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data.trim() == "[DONE]" {
            break;
        }
        if let StreamChunk::Chat(chunk) = serde_json::from_str(data).unwrap() {
            saw_flex |= chunk.service_tier.as_deref() == Some("flex");
            saw_cache_write |= chunk
                .usage
                .as_ref()
                .is_some_and(|usage| usage.cache_write_tokens() == 2);
        }
    }
    assert!(saw_flex, "stream must preserve provider actual tier");
    assert!(saw_cache_write, "stream must preserve cache-write usage");

    let usage = latest_usage(&server, &organization.id).await;
    assert_eq!(usage.service_tier.as_deref(), Some("flex"));
    assert_eq!(usage.context_band.as_deref(), Some("short"));
    assert_eq!(usage.cache_write_tokens, 2);
    let snapshot = usage.billing_details.expect("billing snapshot");
    assert_eq!(snapshot["requestedTier"], "flex");
    assert_eq!(snapshot["actualTier"], "flex");
    assert_eq!(snapshot["rates"]["cacheWrite"], "3.125");
}

#[tokio::test]
async fn responses_rejects_non_standard_processing_tiers() {
    ensure_env();
    let server = setup_test_server().await;
    let organization = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, organization.id).await;
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": CANONICAL_MODEL,
            "input": "hello",
            "service_tier": "flex"
        }))
        .await;
    assert_eq!(response.status_code(), 400, "{}", response.text());
    assert!(response.text().contains("service_tier"));
}
