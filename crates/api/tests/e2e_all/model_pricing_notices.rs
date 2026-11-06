// E2E tests for the pricing-change notice (`x-model-pricing-change-date`, `upcoming_pricing`
// in the catalogs) and for the scheduler pass that applies due changes. Changes are
// scheduled by cloud-platform; here they are seeded and the kept readers and the scheduler
// are what is under test.
//
// The `serial_` tests touch the database-wide due queue or write malformed pending rows,
// so they are an exclusive group in `.config/nextest.toml`.

use crate::common::*;
use chrono::{Duration, Timelike, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use services::models::{ModelCaller, ModelsServiceTrait};
use std::{panic::AssertUnwindSafe, sync::Arc};

const HEADER: &str = "x-model-pricing-change-date";

fn future_notice_date(days: i64) -> chrono::DateTime<Utc> {
    (Utc::now() + Duration::days(days))
        .with_minute(0)
        .and_then(|time| time.with_second(0))
        .and_then(|time| time.with_nanosecond(0))
        .unwrap()
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
    notice_date: chrono::DateTime<Utc>,
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
        let mut batch = api::models::BatchUpdateModelApiRequest::new();
        batch.insert(
            model.clone(),
            serde_json::from_value(json!({
                "inputCostPerToken": { "amount": 1000, "currency": "USD" },
                "outputCostPerToken": { "amount": 2000, "currency": "USD" },
                "modelDisplayName": "Pricing notice fixture",
                "modelDescription": "Synthetic model",
                "contextLength": 4096, "maxOutputLength": 1024,
                "verifiable": false, "isActive": true, "aliases": [&alias],
            }))
            .unwrap(),
        );
        seed_models(&server, batch, get_session_id()).await;
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

    /// Schedule a pending change at the fixture's notice date.
    async fn schedule(
        &self,
        new_input: Option<i64>,
        new_output: Option<i64>,
        new_cache_read: Option<i64>,
        text_pricing: Option<Value>,
    ) -> uuid::Uuid {
        seeding::seed_pricing_change(
            &self.model,
            self.notice_date,
            new_input,
            new_output,
            new_cache_read,
            text_pricing,
        )
        .await
    }

    fn notice(&self) -> String {
        self.notice_date.format("%Y-%m-%dT%H:%M:%SZ").to_string()
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
async fn pending_change_is_announced_on_canonical_alias_and_catalogs() {
    let fixture = Fixture::new().await;
    // Warm cached model entries that have no pending pricing notice.
    fixture.assert_absent().await;
    fixture.schedule(Some(3000), None, Some(0), None).await;
    for alias in [false, true] {
        for stream in [false, true] {
            let response = fixture.chat(alias, stream).await;
            assert_eq!(response.headers()[HEADER], fixture.notice());
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
    assert_eq!(openai["upcoming_pricing"]["effective_at"], fixture.notice());
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
    assert_eq!(native["upcomingPricing"]["effectiveAt"], fixture.notice());
    assert_eq!(
        native["upcomingPricing"]["pricing"]["inputCostPerToken"]["amount"],
        3000
    );
    assert_eq!(
        native["upcomingPricing"]["pricing"]["cacheReadCostPerToken"]["amount"],
        0
    );
}

#[tokio::test]
async fn notices_cover_non_chat_successes_but_not_errors() {
    let fixture = Fixture::new().await;
    fixture.schedule(Some(3000), None, None, None).await;
    let response = fixture
        .server
        .post("/v1/embeddings")
        .add_header("Authorization", format!("Bearer {}", fixture.api_key))
        .json(&json!({"model": fixture.model, "input": "hello"}))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(response.headers()[HEADER], fixture.notice());
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
async fn serial_scheduler_replaces_the_notice_with_the_announced_tiered_prices() {
    let fixture = Fixture::new().await;
    let profile = text_pricing_profile();
    fixture.assert_absent().await;
    let id = fixture
        .schedule(Some(1500), Some(4000), Some(250), Some(profile.clone()))
        .await;
    assert_eq!(
        fixture.chat(true, false).await.headers()[HEADER],
        fixture.notice()
    );
    let (before_openai, before_native) = fixture.catalogs().await;
    assert_eq!(before_openai["upcoming_pricing"]["textPricing"], profile);
    assert_eq!(
        before_native["upcomingPricing"]["pricing"]["textPricing"],
        profile
    );

    seeding::seed_pricing_change_due(id).await;
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

#[tokio::test]
async fn serial_concurrent_schedulers_apply_each_due_change_once() {
    let mut models_service = None;
    let (server, database) = setup_test_server_with_domain_services(|services, _| {
        models_service = Some(services.models_service.clone());
    })
    .await;
    let models_service = models_service.unwrap();
    let mut models = Vec::new();
    let mut change_ids = Vec::new();
    for i in 0..5 {
        let name = format!("pricing-change-race-{i}-{}", uuid::Uuid::new_v4());
        let mut batch = api::models::BatchUpdateModelApiRequest::new();
        batch.insert(
            name.clone(),
            serde_json::from_value(json!({
                "inputCostPerToken": { "amount": 1000, "currency": "USD" },
                "outputCostPerToken": { "amount": 2000, "currency": "USD" },
                "modelDisplayName": "Pricing change race",
                "modelDescription": "Synthetic model",
                "contextLength": 4096, "maxOutputLength": 1024,
                "verifiable": false, "isActive": true,
            }))
            .unwrap(),
        );
        seed_models(&server, batch, get_session_id()).await;
        change_ids.push(
            seeding::seed_pricing_change(
                &name,
                future_notice_date(30),
                Some(1500),
                None,
                None,
                None,
            )
            .await,
        );
        models.push(name);
    }
    let scheduler = || {
        services::admin::ModelPricingScheduler::new(
            Arc::new(database::repositories::AdminCompositeRepository::new(
                database.pool().clone(),
            )),
            models_service.clone(),
        )
    };
    let client = database.pool().get().await.unwrap();
    let history_len = |name: String| {
        let client = &client;
        async move {
            let count: i64 = client
                .query_one(
                    "SELECT COUNT(*) FROM model_history h JOIN models m ON m.id = h.model_id
                     WHERE m.model_name = $1",
                    &[&name],
                )
                .await
                .unwrap()
                .get(0);
            count
        }
    };
    let mut before = Vec::new();
    for name in &models {
        before.push(history_len(name.clone()).await);
    }

    // Not due yet: a pass leaves every change pending.
    scheduler().run_once().await.unwrap();
    for id in &change_ids {
        let status: String = client
            .query_one(
                "SELECT status FROM scheduled_model_pricing_changes WHERE id = $1",
                &[id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(status, "pending");
    }

    // Two scheduler instances racing over the same due set (multi-instance
    // deployment): SKIP LOCKED claims must partition it.
    for id in &change_ids {
        seeding::seed_pricing_change_due(*id).await;
    }
    let (a, b) = (scheduler(), scheduler());
    let (a, b) = tokio::join!(a.run_once(), b.run_once());
    a.unwrap();
    b.unwrap();

    for ((name, id), before) in models.iter().zip(&change_ids).zip(before) {
        let row = client
            .query_one(
                "SELECT c.status, m.input_cost_per_token FROM scheduled_model_pricing_changes c
                 JOIN models m ON m.id = c.model_id WHERE c.id = $1",
                &[id],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "applied", "{name}");
        assert_eq!(row.get::<_, i64>(1), 1500, "{name}");
        // One history entry for the single apply; a double apply would add two.
        assert_eq!(history_len(name.clone()).await, before + 1, "{name}");
    }
}

async fn assert_invalid_pending_profile_is_advisory(profile: Value) {
    let fixture = Fixture::new().await;
    let id = fixture
        .schedule(
            Some(1500),
            Some(4000),
            Some(250),
            Some(text_pricing_profile()),
        )
        .await;
    // Simulate a newer writer or an out-of-band edit, bypassing validation.
    seeding::seed_pricing_change_raw_text_pricing(id, &profile).await;
    // These serial_ tests use the nextest exclusive group: malformed rows
    // would otherwise also affect unrelated tests that list models.
    let result = AssertUnwindSafe(async {
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
    seeding::seed_pricing_change_raw_text_pricing(id, &text_pricing_profile()).await;
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
