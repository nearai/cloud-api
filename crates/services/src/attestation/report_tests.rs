//! Parameter handling for `get_attestation_report`: `signing_algo` is
//! normalized once and that value is what reaches the model backend, and an
//! unknown model is a client error rather than a provider failure. The
//! `MockProvider` backend rejects non-lowercase algorithms the same way
//! inference-proxy does.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use config::ExternalProvidersConfig;
use inference_providers::mock::MockProvider;
use uuid::Uuid;

use super::{
    chat_signature_lifecycle_tests::{lifecycle_service, RecordingRepository},
    models::DstackCpuQuote,
    ports::AttestationServiceTrait,
    AttestationError, AttestationService, GatewayQuoteCollector, GatewayQuoteInput,
};
use crate::{
    inference_provider_pool::InferenceProviderPool,
    models::{ModelWithPricing, ModelsRepository},
};

const MODEL: &str = "test-org/attested-model";
const NONCE: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
/// Mock report `signing_public_key` for an explicit `ed25519` request.
const MOCK_ED25519_PUBLIC_KEY: &str =
    "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// Echoes the requested algorithm like the dstack collector, without a socket.
struct EchoGatewayQuoteCollector;

#[async_trait]
impl GatewayQuoteCollector for EchoGatewayQuoteCollector {
    async fn collect_gateway_quote(
        &self,
        input: GatewayQuoteInput,
    ) -> Result<DstackCpuQuote, AttestationError> {
        Ok(DstackCpuQuote {
            signing_address: input.signing_address,
            signing_algo: input.signing_algo,
            intel_quote: "00".to_string(),
            event_log: "[]".to_string(),
            report_data: hex::encode(input.report_data),
            request_nonce: input.request_nonce,
            info: serde_json::json!({}),
            vpc: input.vpc,
            tls_cert_fingerprint: input.tls_cert_fingerprint,
        })
    }
}

/// Catalog holding only `MODEL`; any other identifier is unknown.
struct SingleModelRepository;

#[async_trait]
impl ModelsRepository for SingleModelRepository {
    async fn get_all_active_models(&self) -> anyhow::Result<Vec<ModelWithPricing>> {
        Ok(vec![catalog_model()])
    }

    async fn get_model_by_name(
        &self,
        model_name: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok((model_name == MODEL).then(catalog_model))
    }

    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok((identifier == MODEL).then(catalog_model))
    }

    async fn get_configured_model_names(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![MODEL.to_string()])
    }
}

fn catalog_model() -> ModelWithPricing {
    ModelWithPricing {
        id: Uuid::nil(),
        model_name: MODEL.to_string(),
        model_display_name: "Attested".to_string(),
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

/// Service whose pool serves `MODEL` from a recording `MockProvider`. Returns
/// the number of attestation calls pool registration already made, so tests
/// can look only at the calls their own requests caused.
async fn report_service() -> (AttestationService, Arc<MockProvider>, usize) {
    let pool = Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ));
    let provider = Arc::new(MockProvider::new());
    pool.register_provider(MODEL.to_string(), provider.clone())
        .await;
    let registration_calls = provider.attestation_signing_algos().len();

    let mut service = lifecycle_service(Arc::new(RecordingRepository::default()), pool);
    service.models_repository = Arc::new(SingleModelRepository);
    service.gateway_quote_collector = Arc::new(EchoGatewayQuoteCollector);
    (service, provider, registration_calls)
}

fn backend_algos_since(provider: &MockProvider, baseline: usize) -> Vec<Option<String>> {
    provider.attestation_signing_algos()[baseline..].to_vec()
}

#[tokio::test]
async fn mixed_case_signing_algo_is_forwarded_lowercased() {
    let (service, provider, baseline) = report_service().await;

    for (requested, normalized) in [("ECDSA", "ecdsa"), ("Ed25519", "ed25519")] {
        let report = service
            .get_attestation_report(
                Some(MODEL.to_string()),
                Some(requested.to_string()),
                Some(NONCE.to_string()),
                None,
                false,
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("{requested} must be accepted: {e}"));

        assert_eq!(report.gateway_attestation.signing_algo, normalized);
        assert_eq!(report.model_attestations.len(), 1);
    }
    assert_eq!(
        backend_algos_since(&provider, baseline),
        vec![Some("ecdsa".to_string()), Some("ed25519".to_string())]
    );
}

#[tokio::test]
async fn omitted_signing_algo_leaves_the_backend_default() {
    let (service, provider, baseline) = report_service().await;

    let report = service
        .get_attestation_report(
            Some(MODEL.to_string()),
            None,
            Some(NONCE.to_string()),
            None,
            false,
            None,
        )
        .await
        .expect("omitted signing_algo must succeed");

    assert_eq!(report.gateway_attestation.signing_algo, "ed25519");
    assert_eq!(backend_algos_since(&provider, baseline), vec![None]);
}

#[tokio::test]
async fn unknown_model_is_a_client_error_and_skips_the_backend() {
    let (service, provider, baseline) = report_service().await;

    for nonce in [Some(NONCE.to_string()), None] {
        let error = match service
            .get_attestation_report(
                Some("test-org/no-such-model".to_string()),
                Some("ecdsa".to_string()),
                nonce,
                None,
                false,
                None,
            )
            .await
        {
            Ok(_) => panic!("unknown model must be rejected"),
            Err(error) => error,
        };

        assert!(
            matches!(&error, AttestationError::UnknownModel(model) if model == "test-org/no-such-model"),
            "unexpected error: {error:?}"
        );
        assert_eq!(
            error.to_string(),
            "Model 'test-org/no-such-model' not found. It's not a valid model name or alias."
        );
    }
    assert!(backend_algos_since(&provider, baseline).is_empty());
}

#[tokio::test]
async fn unsupported_signing_algo_is_rejected_before_the_backend() {
    let (service, provider, baseline) = report_service().await;

    let result = service
        .get_attestation_report(
            Some(MODEL.to_string()),
            Some("RSA".to_string()),
            Some(NONCE.to_string()),
            None,
            false,
            None,
        )
        .await;

    assert!(matches!(result, Err(AttestationError::InvalidParameter(_))));
    assert!(backend_algos_since(&provider, baseline).is_empty());
}

#[tokio::test]
async fn no_nonce_cache_is_keyed_on_the_normalized_algo() {
    let (mut service, provider, baseline) = report_service().await;
    service.report_cache = Some(
        moka::future::Cache::builder()
            .max_capacity(16)
            .time_to_live(Duration::from_secs(60))
            .build(),
    );
    let fetch = |algo: Option<&str>| {
        service.get_attestation_report(
            Some(MODEL.to_string()),
            algo.map(str::to_string),
            None,
            None,
            false,
            None,
        )
    };

    // Casings of one algorithm share an entry: one backend call.
    fetch(Some("ECDSA")).await.expect("ECDSA report");
    fetch(Some("ecdsa")).await.expect("ecdsa report");
    // An omitted algorithm gets the backend default, so a later explicit
    // ed25519 request must not be served that report.
    fetch(None).await.expect("default-algo report");
    let ed25519 = fetch(Some("ed25519")).await.expect("ed25519 report");

    assert_eq!(
        backend_algos_since(&provider, baseline),
        vec![Some("ecdsa".to_string()), None, Some("ed25519".to_string())]
    );
    assert_eq!(
        ed25519.model_attestations[0]["signing_public_key"],
        MOCK_ED25519_PUBLIC_KEY
    );
}
