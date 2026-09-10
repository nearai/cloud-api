//! Regression tests for the signing algorithm forwarded to the inference
//! provider pool by `get_attestation_report_impl`.
//!
//! cloud-api defaults a missing `signing_algo` to `ed25519` (in
//! `normalize_signing_algo`, in `report_cache_key`, and in the gateway quote),
//! but inference-proxy defaults a missing algo to `ecdsa`. Forwarding the
//! caller's raw `Option` therefore produced a report whose gateway attestation
//! was Ed25519 while its model attestation carried the model's **ECDSA** key —
//! and, for nonce-less requests, cached that mismatch under an `a=ed25519` key
//! so subsequent explicit `signing_algo=ed25519` requests were served the ECDSA
//! key too. E2EE then failed with a 64-byte key where 32 bytes were expected.
//!
//! `MockProvider::get_attestation_report` mirrors the production shapes: a
//! 128-hex (64-byte) key for `Some("ecdsa")` and for `None`, a 64-hex (32-byte)
//! key for `Some("ed25519")` — so it reproduces the bug without extra mocking.

use std::sync::Arc;

use async_trait::async_trait;
use config::{ExternalProvidersConfig, ItaAttestationConfig};
use inference_providers::mock::MockProvider;
use serde_json::json;
use uuid::Uuid;

use super::{
    chat_signature_lifecycle_tests::{NoopMetricsService, NoopUsageRepository},
    ita::ProviderPoolModelAttestationCollector,
    models::{AttestationReport, ChatSignature, DstackCpuQuote},
    ports::AttestationRepository,
    AttestationError, AttestationService, GatewayQuoteCollector, GatewayQuoteInput,
};
use crate::{
    inference_provider_pool::InferenceProviderPool,
    models::{ModelWithPricing, ModelsRepository},
};

const TEST_MODEL: &str = "nearai/test-model";

/// Ed25519 public keys are 32 bytes (64 hex chars); ECDSA uncompressed points
/// are 64 bytes (128 hex chars).
const ED25519_PUBKEY_HEX_LEN: usize = 64;
const ECDSA_PUBKEY_HEX_LEN: usize = 128;

struct NoopAttestationRepository;

#[async_trait]
impl AttestationRepository for NoopAttestationRepository {
    async fn add_chat_signature(
        &self,
        _chat_id: &str,
        _signature: ChatSignature,
    ) -> Result<(), AttestationError> {
        Ok(())
    }

    async fn get_chat_signature(
        &self,
        _chat_id: &str,
        _signing_algo: &str,
    ) -> Result<ChatSignature, AttestationError> {
        Err(AttestationError::SignatureNotFound("test".to_string()))
    }
}

/// Resolves exactly `TEST_MODEL` to itself. `EmptyModelsRepository` in the
/// lifecycle tests resolves nothing, which would short-circuit the report
/// build before the provider pool is ever called.
struct SingleModelRepository;

#[async_trait]
impl ModelsRepository for SingleModelRepository {
    async fn get_all_active_models(&self) -> anyhow::Result<Vec<ModelWithPricing>> {
        Ok(vec![test_model()])
    }

    async fn get_model_by_name(
        &self,
        model_name: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok((model_name == TEST_MODEL).then(test_model))
    }

    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok((identifier == TEST_MODEL).then(test_model))
    }

    async fn get_configured_model_names(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![TEST_MODEL.to_string()])
    }
}

fn test_model() -> ModelWithPricing {
    ModelWithPricing {
        id: Uuid::new_v4(),
        model_name: TEST_MODEL.to_string(),
        model_display_name: "Test Model".to_string(),
        model_description: String::new(),
        model_icon: None,
        input_cost_per_token: 0,
        output_cost_per_token: 0,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        text_pricing: None,
        context_length: 4096,
        verifiable: true,
        aliases: Vec::new(),
        owned_by: "test".to_string(),
        provider_type: "vllm".to_string(),
        provider_config: None,
        attestation_supported: true,
        input_modalities: None,
        output_modalities: None,
        inference_url: None,
        hugging_face_id: None,
        quantization: None,
        max_output_length: None,
        supported_sampling_parameters: Vec::new(),
        supported_features: Vec::new(),
        datacenters: None,
        is_ready: None,
        deprecation_date: None,
        openrouter_slug: None,
        created_at: chrono::Utc::now(),
    }
}

/// Stands in for `DstackGatewayQuoteCollector`, which needs a live dstack Unix
/// socket. The report build joins the gateway quote with the model attestation,
/// so the quote has to succeed for the model attestation to be observable.
struct StubGatewayQuoteCollector;

#[async_trait]
impl GatewayQuoteCollector for StubGatewayQuoteCollector {
    async fn collect_gateway_quote(
        &self,
        input: GatewayQuoteInput,
    ) -> Result<DstackCpuQuote, AttestationError> {
        Ok(DstackCpuQuote {
            signing_address: input.signing_address,
            signing_algo: input.signing_algo,
            intel_quote: "0x01020304".to_string(),
            event_log: "[]".to_string(),
            report_data: hex::encode(input.report_data),
            request_nonce: input.request_nonce,
            info: json!({}),
            vpc: input.vpc,
            tls_cert_fingerprint: input.tls_cert_fingerprint,
        })
    }
}

/// Service wired to a pool holding a single `MockProvider` for `TEST_MODEL`.
/// `report_cache: None` so every call takes the cache-bypass path and the
/// assertions are about the forwarded algo, not about cache behaviour.
async fn service_with_mock_provider() -> AttestationService {
    let pool = Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ));
    pool.register_provider(TEST_MODEL.to_string(), Arc::new(MockProvider::new()))
        .await;

    let (ed25519_signing_key, ed25519_verifying_key, ecdsa_signing_key, ecdsa_verifying_key) =
        AttestationService::generate_ephemeral_signing_keys();
    AttestationService {
        repository: Arc::new(NoopAttestationRepository),
        inference_provider_pool: pool.clone(),
        models_repository: Arc::new(SingleModelRepository),
        metrics_service: Arc::new(NoopMetricsService),
        usage_repository: Arc::new(NoopUsageRepository),
        vpc_info: None,
        vpc_shared_secret: None,
        tls_cert_fingerprint: None,
        ed25519_signing_key: Arc::new(ed25519_signing_key),
        ed25519_verifying_key: Arc::new(ed25519_verifying_key),
        ecdsa_signing_key: Arc::new(ecdsa_signing_key),
        ecdsa_verifying_key: Arc::new(ecdsa_verifying_key),
        ita_config: ItaAttestationConfig::default(),
        ita_client: None,
        gateway_quote_collector: Arc::new(StubGatewayQuoteCollector),
        model_attestation_collector: Arc::new(ProviderPoolModelAttestationCollector::new(pool)),
        report_cache: None,
    }
}

async fn report_for(signing_algo: Option<&str>) -> AttestationReport {
    service_with_mock_provider()
        .await
        .get_attestation_report_impl(
            Some(TEST_MODEL.to_string()),
            signing_algo.map(str::to_string),
            None,
            None,
            false,
            None,
        )
        .await
        .expect("attestation report build should succeed")
}

fn model_signing_public_key(report: &AttestationReport) -> String {
    report
        .model_attestations
        .first()
        .expect("mock provider returns exactly one model attestation")
        .get("signing_public_key")
        .and_then(|v| v.as_str())
        .expect("model attestation carries signing_public_key")
        .to_string()
}

#[tokio::test]
async fn omitted_signing_algo_returns_the_ed25519_model_key() {
    // Regression: the raw `None` used to reach the provider pool, which made
    // inference-proxy fall back to its own `ecdsa` default while the rest of
    // the report (cache key + gateway quote) already used `ed25519`.
    let report = report_for(None).await;
    let key = model_signing_public_key(&report);

    assert_eq!(
        key.len(),
        ED25519_PUBKEY_HEX_LEN,
        "omitting signing_algo must yield the Ed25519 model key (64 hex chars), got {} hex chars: {key}",
        key.len()
    );
    // The whole report must agree on the algorithm it was built for.
    assert_eq!(report.gateway_attestation.signing_algo, "ed25519");
}

#[tokio::test]
async fn explicit_ed25519_returns_the_ed25519_model_key() {
    let report = report_for(Some("ed25519")).await;
    let key = model_signing_public_key(&report);

    assert_eq!(key.len(), ED25519_PUBKEY_HEX_LEN, "got {key}");
    assert_eq!(report.gateway_attestation.signing_algo, "ed25519");
}

#[tokio::test]
async fn explicit_ecdsa_returns_the_ecdsa_model_key() {
    let report = report_for(Some("ecdsa")).await;
    let key = model_signing_public_key(&report);

    assert_eq!(key.len(), ECDSA_PUBKEY_HEX_LEN, "got {key}");
    assert_eq!(report.gateway_attestation.signing_algo, "ecdsa");
}

#[tokio::test]
async fn uppercase_signing_algo_is_normalized_before_forwarding() {
    // `normalize_signing_algo` lowercases; the provider must see the
    // lowercased value, not the caller's casing.
    let report = report_for(Some("ED25519")).await;
    let key = model_signing_public_key(&report);

    assert_eq!(key.len(), ED25519_PUBKEY_HEX_LEN, "got {key}");
    assert_eq!(report.gateway_attestation.signing_algo, "ed25519");
}
