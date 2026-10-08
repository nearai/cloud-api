//! E2E for Tinfoil as an attested fallback (`ProviderSource::Tinfoil`).
//!
//! The real provider needs the live router, so the pool is fed a mock tagged
//! `Attested3p` / `Tinfoil` and registered as a pinned fallback behind a
//! failing NEAR mock; the production registration path is covered by unit
//! tests in `attested_3p_startup`. Every model name is UUID-scoped.
//!
//! The `serial_` tests flip the global `attested_3p` kill switch, so they are
//! a serialized override in `.config/nextest.toml`.

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;
use bytes::Bytes;
use inference_providers::{mock::MockProvider, CompletionError, ProviderSource, ProviderTier};
use serde_json::{json, Value};
use std::sync::Arc;

const SETTING_PATH: &str = "/v1/admin/settings/attested_3p";

struct Fixture {
    server: axum_test::TestServer,
    database: Arc<database::Database>,
    model: String,
    near: Arc<MockProvider>,
    tinfoil: Arc<MockProvider>,
    chutes: Option<Arc<MockProvider>>,
    api_key: String,
}

async fn fixture(with_chutes: bool) -> Fixture {
    let (server, pool, _mock, database) = setup_test_server_with_pool().await;
    let model = format!("nearai/test-tinfoil-{}", uuid::Uuid::new_v4());
    let near = Arc::new(
        MockProvider::new_accept_all()
            .with_tier(ProviderTier::Near)
            .with_provider_source(ProviderSource::Vllm),
    );
    near.set_error_override(Some(CompletionError::HttpError {
        status_code: 503,
        message: "NEAR unavailable".to_string(),
        is_external: true,
    }))
    .await;
    pool.register_provider(model.clone(), near.clone()).await;

    let attested = |source| {
        Arc::new(
            MockProvider::new_accept_all()
                .with_tier(ProviderTier::Attested3p)
                .with_provider_source(source)
                .with_client_e2ee_support(false)
                .with_chat_signature_support(false),
        )
    };
    let chutes = with_chutes.then(|| attested(ProviderSource::Chutes));
    if let Some(c) = &chutes {
        pool.register_pinned_secondary_provider(model.clone(), c.clone(), None)
            .await;
    }
    let tinfoil = attested(ProviderSource::Tinfoil);
    pool.register_pinned_secondary_provider(model.clone(), tinfoil.clone(), None)
        .await;

    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model.clone(),
        serde_json::from_value(json!({
            "inputCostPerToken": { "amount": 1_000_000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2_000_000, "currency": "USD" },
            "modelDisplayName": "Tinfoil fixture",
            "modelDescription": "Tinfoil attested fallback test model",
            "contextLength": 128000,
            "maxOutputLength": 1024,
            "verifiable": true,
            "isActive": true,
            "attestationSupported": true,
            "providerType": "vllm"
        }))
        .expect("fixture should deserialize"),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    Fixture {
        server,
        database,
        model,
        near,
        tinfoil,
        chutes,
        api_key,
    }
}

impl Fixture {
    fn chat(&self, stream: bool, max_tokens: i64) -> axum_test::TestRequest {
        self.server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {}", self.api_key))
            .json(&json!({
                "model": self.model,
                "messages": [{"role": "user", "content": "Hello"}],
                "stream": stream,
                "max_tokens": max_tokens
            }))
    }

    async fn report(&self, provider: &str) -> axum_test::TestResponse {
        let model = url::form_urlencoded::byte_serialize(self.model.as_bytes()).collect::<String>();
        self.server
            .get(&format!(
                "/v1/attestation/report?model={model}&provider={provider}"
            ))
            .add_header("Authorization", format!("Bearer {}", self.api_key))
            .await
    }

    async fn set_disabled(&self, sources: Value) {
        let r = self
            .server
            .patch(SETTING_PATH)
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&json!({ "disabled_sources": sources }))
            .await;
        assert_eq!(r.status_code(), 200, "{}", r.text());
    }
}

#[tokio::test]
async fn serving_header_is_tinfoil_streaming_and_non_streaming() {
    let f = fixture(false).await;
    let r = f.chat(false, 16).await;
    assert_eq!(r.status_code(), 200, "{}", r.text());
    assert_eq!(r.header("x-serving-provider"), "tinfoil");
    let r = f.chat(true, 16).await;
    assert_eq!(r.status_code(), 200, "{}", r.text());
    assert_eq!(r.header("x-serving-provider"), "tinfoil");
    assert!(f.tinfoil.last_chat_params().await.is_some());
}

#[tokio::test]
async fn attestation_report_provider_tinfoil_returns_mock_report() {
    let f = fixture(false).await;
    let r = f.report("tinfoil").await;
    assert_eq!(r.status_code(), 200, "{}", r.text());
    assert!(
        r.text().contains("mock-attestation"),
        "report should come from the Tinfoil mock: {}",
        r.text()
    );
    assert!(!f.tinfoil.attestation_signing_algos().is_empty());
}

#[tokio::test]
async fn gateway_signature_verifies_for_tinfoil_served_response() {
    let f = fixture(false).await;
    let request_json = serde_json::to_string_pretty(&json!({
        "model": f.model,
        "messages": [{ "role": "user", "content": "Respond with two words." }],
        "stream": false,
        "nonce": 1301
    }))
    .unwrap();
    let r = f
        .server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", f.api_key))
        .content_type("application/json")
        .bytes(Bytes::from(request_json.clone()))
        .await;
    assert_eq!(r.status_code(), 200, "{}", r.text());
    assert_eq!(r.header("x-serving-provider"), "tinfoil");
    let response_text = r.text();
    let completion: Value = serde_json::from_str(&response_text).unwrap();
    let chat_id = completion["id"].as_str().expect("completion id");
    crate::signature_verification::assert_gateway_signatures(
        &f.server,
        &f.api_key,
        chat_id,
        &request_json,
        &response_text,
    )
    .await;
}

#[tokio::test]
async fn usage_row_records_tinfoil_as_served_provider() {
    let f = fixture(false).await;
    let r = f.chat(false, 16).await;
    assert_eq!(r.status_code(), 200, "{}", r.text());
    let client = f.database.pool().get().await.unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let rows = client
            .query(
                "SELECT served_provider_type, served_provider_tier, served_via_fallback
                 FROM organization_usage_log WHERE model_name = $1",
                &[&f.model],
            )
            .await
            .unwrap();
        if let Some(row) = rows.first() {
            let ty: Option<String> = row.get(0);
            let tier: Option<String> = row.get(1);
            let fallback: bool = row.get(2);
            assert_eq!(ty.as_deref(), Some("tinfoil"));
            assert_eq!(tier.as_deref(), Some("attested_3p"));
            assert!(fallback, "served behind a failing NEAR primary");
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "usage row never recorded"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn serial_kill_switch_fails_over_to_next_provider() {
    let f = fixture(true).await;
    let chutes = f.chutes.clone().unwrap();
    f.set_disabled(json!(["tinfoil"])).await;
    let r = f.chat(false, 21).await;
    let (status, header, body) = (
        r.status_code(),
        r.maybe_header("x-serving-provider")
            .map(|h| h.to_str().unwrap().to_string()),
        r.text(),
    );
    let tinfoil_called = f.tinfoil.last_chat_params().await.is_some();
    let chutes_called = chutes.last_chat_params().await.is_some();
    // Reset before asserting so a failure cannot leak the setting.
    f.set_disabled(json!(null)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(header.as_deref(), Some("chutes"));
    assert!(!tinfoil_called, "a disabled source must not be invoked");
    assert!(chutes_called);
}

#[tokio::test]
async fn serial_kill_switch_with_no_provider_left_errors_and_blocks_report() {
    let f = fixture(false).await;
    f.set_disabled(json!(["tinfoil"])).await;
    let r = f.chat(false, 22).await;
    let (status, body) = (r.status_code(), r.text());
    let report = f.report("tinfoil").await;
    let (report_status, report_body) = (report.status_code(), report.text());
    let tinfoil_chat = f.tinfoil.last_chat_params().await.is_some();
    let tinfoil_report_calls = f.tinfoil.attestation_signing_algos().len();
    f.set_disabled(json!(null)).await;

    // NEAR's 503 is all that is left (same shape as the Chutes kill-switch test).
    assert_eq!(status.as_u16(), 429, "{body}");
    assert!(f.near.last_chat_params().await.is_some());
    assert!(!tinfoil_chat, "a disabled source must not be invoked");
    assert_eq!(report_status.as_u16(), 503, "{report_body}");
    assert_eq!(
        tinfoil_report_calls, 0,
        "report must not reach a disabled source"
    );
}
