use crate::common::*;
use chrono::{Duration, Timelike, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use services::models::{ModelCaller, ModelsServiceTrait};
use std::{panic::AssertUnwindSafe, sync::Arc};

const HEADER: &str = "x-model-pricing-change-date";

fn future_notice_date(days: i64) -> String {
    (Utc::now() + Duration::days(days))
        .with_minute(0)
        .and_then(|time| time.with_second(0))
        .and_then(|time| time.with_nanosecond(0))
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn text_pricing_profile() -> Value {
    json!({
        "version": 1, "currency": "USD", "unit": "million_tokens",
        "longContextThreshold": 1000,
        "tiers": {"default": {
            "short": {"uncachedInput": "1.5", "cachedInput": "0.25", "output": "4"},
            "long": {"uncachedInput": "3", "cachedInput": "0.5", "output": "8"}
        }}
    })
}

struct Fixture {
    server: axum_test::TestServer,
    database: Arc<database::Database>,
    models: Arc<services::models::ModelsServiceImpl>,
    model: String,
    alias: String,
    api_key: String,
    notice_date: String,
}

impl Fixture {
    async fn new() -> Self {
        let mut models = None;
        let (server, database) = setup_test_server_with_domain_services(|services, _| {
            models = Some(services.models_service.clone());
        })
        .await;
        let models = models.unwrap();
        let suffix = uuid::Uuid::new_v4();
        let model = format!("pricing-notice-{suffix}");
        let alias = format!("pricing-notice-alias-{suffix}");
        let response = server
            .patch("/v1/admin/models")
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&json!({ &model: {
                "inputCostPerToken": { "amount": 1000, "currency": "USD" },
                "outputCostPerToken": { "amount": 2000, "currency": "USD" },
                "modelDisplayName": "Pricing notice fixture",
                "modelDescription": "Synthetic model",
                "contextLength": 4096, "maxOutputLength": 1024,
                "verifiable": false, "isActive": true, "aliases": [&alias],
            } }))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        models
            .inference_provider_pool
            .register_provider(
                model.clone(),
                Arc::new(inference_providers::mock::MockProvider::new_accept_all()),
            )
            .await;
        let org = setup_org_with_credits(&server, 10_000_000_000).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        Self {
            server,
            database,
            models,
            model,
            alias,
            api_key,
            notice_date: future_notice_date(90),
        }
    }

    async fn chat(&self, alias: bool, stream: bool) -> axum_test::TestResponse {
        let response = self
            .server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {}", self.api_key))
            .add_header("Origin", "https://client.example")
            .json(&json!({
                "model": if alias { &self.alias } else { &self.model },
                "messages": [{"role": "user", "content": "Hello"}],
                "max_tokens": 16, "stream": stream,
            }))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        response
    }

    async fn confirm(&self, mut fields: Value) -> Value {
        fields["modelId"] = json!(self.model);
        if fields.get("effectiveAt").is_none() {
            fields["effectiveAt"] = json!(self.notice_date);
        }
        let response = self
            .server
            .post("/v1/admin/models/pricing-changes/confirm")
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&json!({"changes": [fields]}))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        response.json::<Value>()["changes"][0].clone()
    }

    async fn cancel(&self, change: &Value) {
        let response = self
            .server
            .delete(&format!(
                "/v1/admin/models/pricing-changes/{}",
                change["id"].as_str().unwrap()
            ))
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
    }

    async fn catalogs(&self) -> (Value, Value) {
        let openai = self.server.get("/v1/models").await;
        assert_eq!(openai.status_code(), 200, "{}", openai.text());
        let native = self.server.get("/v1/model/list?limit=10000").await;
        assert_eq!(native.status_code(), 200, "{}", native.text());
        let openai: Value = openai.json();
        let native: Value = native.json();
        (
            openai["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == self.model)
                .unwrap()
                .clone(),
            native["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["modelId"] == self.model)
                .unwrap()
                .clone(),
        )
    }

    async fn assert_absent(&self) {
        for alias in [false, true] {
            assert!(!self.chat(alias, false).await.headers().contains_key(HEADER));
        }
        let (openai, native) = self.catalogs().await;
        assert!(openai.get("upcoming_pricing").is_none(), "{openai}");
        assert!(native.get("upcomingPricing").is_none(), "{native}");
    }
}

#[tokio::test]
async fn confirm_and_cancel_refresh_warm_canonical_alias_and_catalog_caches() {
    let fixture = Fixture::new().await;
    // Warm cached model entries that have no pending pricing notice.
    fixture.assert_absent().await;
    let change = fixture
        .confirm(json!({
            "inputCostPerToken": {"amount": 3000, "currency": "USD"},
            "cacheReadCostPerToken": {"amount": 0, "currency": "USD"},
        }))
        .await;
    for alias in [false, true] {
        for stream in [false, true] {
            let response = fixture.chat(alias, stream).await;
            assert_eq!(response.headers()[HEADER], fixture.notice_date);
            // Pricing-only notices must work without any deprecation date.
            assert!(!response.headers().contains_key("x-model-deprecation-date"));
            assert_eq!(response.headers()["access-control-expose-headers"], "*");
            if !stream {
                let body: Value = response.json();
                assert!(body.get("upcoming_pricing").is_none());
                assert_eq!(body.get("warning").is_some(), alias, "{body}");
            }
        }
    }
    let (openai, native) = fixture.catalogs().await;
    assert_eq!(openai["pricing"]["prompt"], "0.000001");
    assert_eq!(
        openai["upcoming_pricing"]["effective_at"],
        fixture.notice_date
    );
    assert_eq!(openai["upcoming_pricing"]["pricing"]["prompt"], "0.000003");
    assert_eq!(
        openai["upcoming_pricing"]["pricing"]["completion"],
        "0.000002"
    );
    assert_eq!(
        openai["upcoming_pricing"]["pricing"]["input_cache_read"],
        "0"
    );
    assert_eq!(native["inputCostPerToken"]["amount"], 1000);
    assert_eq!(
        native["upcomingPricing"]["effectiveAt"],
        fixture.notice_date
    );
    assert_eq!(
        native["upcomingPricing"]["pricing"]["inputCostPerToken"]["amount"],
        3000
    );
    assert_eq!(
        native["upcomingPricing"]["pricing"]["cacheReadCostPerToken"]["amount"],
        0
    );
    fixture.cancel(&change).await;
    fixture.assert_absent().await;

    // The earlier cancelled row must not shadow a later pending change.
    let later_date = future_notice_date(180);
    fixture
        .confirm(json!({
            "effectiveAt": later_date,
            "inputCostPerToken": {"amount": 4000, "currency": "USD"},
        }))
        .await;
    assert_eq!(
        fixture.chat(true, false).await.headers()[HEADER],
        later_date
    );
}

#[tokio::test]
async fn notices_cover_non_chat_successes_but_not_errors() {
    let fixture = Fixture::new().await;
    fixture
        .confirm(json!({"inputCostPerToken": {"amount": 3000, "currency": "USD"}}))
        .await;
    let response = fixture
        .server
        .post("/v1/embeddings")
        .add_header("Authorization", format!("Bearer {}", fixture.api_key))
        .json(&json!({"model": fixture.model, "input": "hello"}))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(response.headers()[HEADER], fixture.notice_date);
    let error = fixture
        .server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", fixture.api_key))
        .json(&json!({"model": fixture.model, "messages": []}))
        .await;
    assert_eq!(error.status_code(), 400, "{}", error.text());
    assert!(!error.headers().contains_key(HEADER));
}

#[tokio::test]
async fn future_prices_use_live_unchanged_fields_and_preserve_disabled_cache_pricing() {
    let fixture = Fixture::new().await;
    fixture
        .confirm(json!({"inputCostPerToken": {"amount": 3000, "currency": "USD"}}))
        .await;
    fixture.catalogs().await;
    let response = fixture
        .server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(
            &json!({ &fixture.model: {"outputCostPerToken": {"amount": 9000, "currency": "USD"}}}),
        )
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let (openai, native) = fixture.catalogs().await;
    assert_eq!(
        openai["upcoming_pricing"]["pricing"]["completion"],
        "0.000009"
    );
    assert!(openai["upcoming_pricing"]["pricing"]
        .get("input_cache_read")
        .is_none());
    assert_eq!(
        native["upcomingPricing"]["pricing"]["outputCostPerToken"]["amount"],
        9000
    );
    assert!(native["upcomingPricing"]["pricing"]
        .get("cacheReadCostPerToken")
        .is_none());
}

#[tokio::test]
async fn serial_scheduler_replaces_the_notice_with_the_announced_tiered_prices() {
    let fixture = Fixture::new().await;
    let profile = text_pricing_profile();
    fixture.assert_absent().await;
    let change = fixture.confirm(json!({"textPricing": profile})).await;
    assert_eq!(
        fixture.chat(true, false).await.headers()[HEADER],
        fixture.notice_date
    );
    let (before_openai, before_native) = fixture.catalogs().await;
    assert_eq!(before_openai["upcoming_pricing"]["textPricing"], profile);
    assert_eq!(
        before_native["upcomingPricing"]["pricing"]["textPricing"],
        profile
    );
    let id = uuid::Uuid::parse_str(change["id"].as_str().unwrap()).unwrap();
    let updated = fixture.database.pool().get().await.unwrap()
        .execute("UPDATE scheduled_model_pricing_changes SET effective_at = NOW() - interval '1 minute' WHERE id = $1 AND status = 'pending'", &[&id]).await.unwrap();
    assert_eq!(
        updated, 1,
        "scheduled pricing change {id} was not backdated"
    );
    let scheduler = services::admin::ModelPricingScheduler::new(
        Arc::new(database::repositories::AdminCompositeRepository::new(
            fixture.database.pool().clone(),
        )),
        fixture.models.clone(),
    );
    scheduler.run_once().await.unwrap();
    fixture.assert_absent().await;
    let (after_openai, after_native) = fixture.catalogs().await;
    assert_eq!(
        after_openai["pricing"],
        before_openai["upcoming_pricing"]["pricing"]
    );
    assert_eq!(after_openai["textPricing"], profile);
    assert_eq!(after_native["textPricing"], profile);
    for field in [
        "inputCostPerToken",
        "outputCostPerToken",
        "cacheReadCostPerToken",
        "costPerImage",
    ] {
        assert_eq!(
            after_native[field],
            before_native["upcomingPricing"]["pricing"][field]
        );
    }
    let resolved = fixture
        .models
        .resolve_and_get_model(&fixture.alias, ModelCaller::Trusted)
        .await
        .unwrap();
    assert!(resolved.upcoming_pricing.is_none());
}

async fn assert_invalid_pending_profile_is_advisory(profile: Value) {
    let fixture = Fixture::new().await;
    let change = fixture
        .confirm(json!({"textPricing": text_pricing_profile()}))
        .await;
    let id = uuid::Uuid::parse_str(change["id"].as_str().unwrap()).unwrap();
    // Simulate a newer writer or an out-of-band edit, bypassing admin validation.
    let updated = fixture.database.pool().get().await.unwrap()
        .execute("UPDATE scheduled_model_pricing_changes SET new_text_pricing = $2 WHERE id = $1 AND status = 'pending'", &[&id, &profile]).await.unwrap();
    assert_eq!(updated, 1, "scheduled pricing change {id} was not updated");
    // These serial_ tests use the existing nextest exclusive group: malformed
    // rows would otherwise also affect unrelated admin listing tests.
    let result = AssertUnwindSafe(async {
        fixture.models.invalidate_models_cache().await;

        // Exercise the canonical-only repository path as well as alias resolution.
        let model = fixture
            .models
            .get_model_by_name(&fixture.model, ModelCaller::Trusted)
            .await
            .unwrap();
        assert!(model.upcoming_pricing.is_none());
        assert_eq!(model.input_cost_per_token, 1000);
        assert_eq!(model.output_cost_per_token, 2000);
        for alias in [false, true] {
            for stream in [false, true] {
                assert!(!fixture
                    .chat(alias, stream)
                    .await
                    .headers()
                    .contains_key(HEADER));
            }
        }
        let (openai, native) = fixture.catalogs().await;
        assert!(openai.get("upcoming_pricing").is_none(), "{openai}");
        assert!(native.get("upcomingPricing").is_none(), "{native}");
        assert_eq!(openai["pricing"]["prompt"], "0.000001");
        assert_eq!(openai["pricing"]["completion"], "0.000002");
        assert_eq!(native["inputCostPerToken"]["amount"], 1000);
        assert_eq!(native["outputCostPerToken"]["amount"], 2000);

        let detail = fixture
            .server
            .get(&format!("/v1/model/{}", fixture.model))
            .await;
        assert_eq!(detail.status_code(), 200, "{}", detail.text());
        assert!(detail.json::<Value>().get("upcomingPricing").is_none());
    })
    .catch_unwind()
    .await;

    // Restore the row even if an assertion panics, so later tests see valid data.
    let restored = fixture
        .database
        .pool()
        .get()
        .await
        .unwrap()
        .execute(
            "UPDATE scheduled_model_pricing_changes SET new_text_pricing = $2 WHERE id = $1",
            &[&id, &text_pricing_profile()],
        )
        .await
        .unwrap();
    assert_eq!(
        restored, 1,
        "scheduled pricing change {id} was not restored"
    );
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn serial_unknown_pending_profile_fields_do_not_break_inference_or_catalogs() {
    let mut profile = text_pricing_profile();
    profile["futureField"] = json!(true);
    assert_invalid_pending_profile_is_advisory(profile).await;
}

#[tokio::test]
async fn serial_unsupported_pending_profile_versions_do_not_break_inference_or_catalogs() {
    let mut profile = text_pricing_profile();
    profile["version"] = json!(2);
    assert_invalid_pending_profile_is_advisory(profile).await;
}

#[tokio::test]
async fn serial_invalid_pending_profile_rates_do_not_break_inference_or_catalogs() {
    let mut profile = text_pricing_profile();
    profile["tiers"]["default"]["short"]["uncachedInput"] = json!("-1");
    assert_invalid_pending_profile_is_advisory(profile).await;
}
