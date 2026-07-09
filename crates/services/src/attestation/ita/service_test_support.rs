// allow: SIZE_OK - test-only service harness needs local fake repositories and collectors.
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use config::{ExternalProvidersConfig, ItaAttestationConfig, ItaBaseUrl, ItaPolicyIds};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::super::{client::client_test_support::FakeIta, ItaClient};
use crate::{
    attestation::{
        models::{ChatSignature, DstackCpuQuote},
        ports::AttestationRepository,
        AttestationError, AttestationService, GatewayQuoteCollector, GatewayQuoteInput,
        ModelAttestationCollector, ModelAttestationInput,
    },
    inference_provider_pool::InferenceProviderPool,
    metrics::MetricsServiceTrait,
    models::{ModelWithPricing, ModelsRepository},
    usage::{
        InferenceCost, OrganizationBalanceInfo, RecordUsageDbRequest, StopReason,
        UsageByModelEntry, UsageLogEntry, UsageRepository,
    },
};

pub(super) fn service_for_fake_ita(
    server: &FakeIta,
    max_retries: u32,
    gateway_collector: RecordingGatewayQuoteCollector,
    model_collector: Option<RecordingModelCollector>,
) -> Result<AttestationService, Box<dyn std::error::Error>> {
    service_for_fake_ita_with_timeout(server, max_retries, 100, gateway_collector, model_collector)
}

pub(super) fn service_for_fake_ita_with_timeout(
    server: &FakeIta,
    max_retries: u32,
    timeout_ms: u64,
    gateway_collector: RecordingGatewayQuoteCollector,
    model_collector: Option<RecordingModelCollector>,
) -> Result<AttestationService, Box<dyn std::error::Error>> {
    let mut config = ita_config(true, &server.base_url())?;
    config.max_retries = max_retries;
    let ita_client = ItaClient::from_config_for_test(&config, Duration::from_millis(timeout_ms))?;
    Ok(service_with_collectors(
        config,
        Some(ita_client),
        gateway_collector,
        model_collector.unwrap_or_else(RecordingModelCollector::compatible),
    ))
}

pub(super) fn service_with_ita_client(
    config: ItaAttestationConfig,
    ita_client: Option<ItaClient>,
) -> AttestationService {
    service_with_collectors(
        config,
        ita_client,
        RecordingGatewayQuoteCollector::default(),
        RecordingModelCollector::compatible(),
    )
}

fn service_with_collectors(
    ita_config: ItaAttestationConfig,
    ita_client: Option<ItaClient>,
    gateway_collector: RecordingGatewayQuoteCollector,
    model_collector: RecordingModelCollector,
) -> AttestationService {
    let (ed25519_signing_key, ed25519_verifying_key, ecdsa_signing_key, ecdsa_verifying_key) =
        AttestationService::generate_ephemeral_signing_keys();
    AttestationService {
        repository: Arc::new(NoopAttestationRepository),
        inference_provider_pool: Arc::new(InferenceProviderPool::new(
            None,
            ExternalProvidersConfig::default(),
        )),
        models_repository: Arc::new(FakeModelsRepository),
        metrics_service: Arc::new(NoopMetricsService),
        usage_repository: Arc::new(NoopUsageRepository),
        vpc_info: None,
        vpc_shared_secret: None,
        tls_cert_fingerprint: None,
        ed25519_signing_key: Arc::new(ed25519_signing_key),
        ed25519_verifying_key: Arc::new(ed25519_verifying_key),
        ecdsa_signing_key: Arc::new(ecdsa_signing_key),
        ecdsa_verifying_key: Arc::new(ecdsa_verifying_key),
        ita_config,
        ita_client,
        gateway_quote_collector: Arc::new(gateway_collector),
        model_attestation_collector: Arc::new(model_collector),
        report_cache: None,
    }
}

pub(super) fn ita_config(
    enabled: bool,
    api_base_url: &str,
) -> Result<ItaAttestationConfig, String> {
    Ok(ItaAttestationConfig {
        enabled,
        api_base_url: ItaBaseUrl::parse(api_base_url, "ITA_API_BASE_URL")?,
        portal_base_url: ItaBaseUrl::parse("https://portal.example.test", "ITA_PORTAL_BASE_URL")?,
        api_key: Some("test-api-key".to_string()),
        timeout_seconds: 1,
        max_retries: 0,
        retry_backoff_ms: 1,
        policy_ids: ItaPolicyIds::default(),
        policy_must_match: false,
        token_signing_alg: Default::default(),
    })
}

#[derive(Clone, Default)]
pub(super) struct RecordingGatewayQuoteCollector {
    calls: Arc<Mutex<Vec<ObservedGatewayQuoteCall>>>,
}

#[derive(Clone)]
pub(super) struct ObservedGatewayQuoteCall {
    pub(super) report_data: Vec<u8>,
}

#[async_trait]
impl GatewayQuoteCollector for RecordingGatewayQuoteCollector {
    async fn collect_gateway_quote(
        &self,
        input: GatewayQuoteInput,
    ) -> Result<DstackCpuQuote, AttestationError> {
        self.calls
            .lock()
            .map_err(|_| {
                AttestationError::InternalError("gateway quote calls poisoned".to_string())
            })?
            .push(ObservedGatewayQuoteCall {
                report_data: input.report_data.clone(),
            });
        Ok(DstackCpuQuote {
            signing_address: input.signing_address,
            signing_algo: input.signing_algo,
            intel_quote: "0x01020304".to_string(),
            event_log: "0x0a0b".to_string(),
            report_data: hex::encode(input.report_data),
            request_nonce: input.request_nonce,
            info: json!({}),
            vpc: input.vpc,
            tls_cert_fingerprint: input.tls_cert_fingerprint,
        })
    }
}

impl RecordingGatewayQuoteCollector {
    pub(super) fn calls(&self) -> Vec<ObservedGatewayQuoteCall> {
        self.calls
            .lock()
            .map(|calls| calls.clone())
            .unwrap_or_default()
    }
}

#[derive(Clone)]
pub(super) struct RecordingModelCollector {
    compatible: bool,
    calls: Arc<Mutex<Vec<String>>>,
}

impl RecordingModelCollector {
    pub(super) fn compatible() -> Self {
        Self {
            compatible: true,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(super) fn incompatible() -> Self {
        Self {
            compatible: false,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl ModelAttestationCollector for RecordingModelCollector {
    async fn collect_model_attestations(
        &self,
        input: ModelAttestationInput,
    ) -> Result<Vec<Map<String, Value>>, AttestationError> {
        self.calls
            .lock()
            .map_err(|_| AttestationError::InternalError("model calls poisoned".to_string()))?
            .push(input.model);
        if !self.compatible {
            return Ok(vec![Map::new()]);
        }
        let gpu_nonce = input.nonce.ok_or_else(|| {
            AttestationError::InternalError("expected ITA GPU nonce for model evidence".to_string())
        })?;
        Ok(vec![model_evidence(&gpu_nonce)])
    }
}

impl RecordingModelCollector {
    pub(super) fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .map(|calls| calls.clone())
            .unwrap_or_default()
    }
}

fn model_evidence(gpu_nonce: &str) -> Map<String, Value> {
    let payload = json!({
        "gpu_nonce": gpu_nonce,
        "arch": "HOPPER",
        "evidence_list": [{ "certificate": "Y2VydA==", "evidence": "ZXZpZGVuY2U=" }]
    });
    let mut evidence = Map::new();
    evidence.insert(
        "nvidia_payload".to_string(),
        Value::String(payload.to_string()),
    );
    evidence
}

struct FakeModelsRepository;

#[async_trait]
impl ModelsRepository for FakeModelsRepository {
    async fn get_all_active_models(&self) -> anyhow::Result<Vec<ModelWithPricing>> {
        Ok(vec![canonical_model()])
    }

    async fn get_model_by_name(
        &self,
        model_name: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok((model_name == "canonical-model").then(canonical_model))
    }

    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok((identifier == "alias-model" || identifier == "canonical-model").then(canonical_model))
    }

    async fn get_configured_model_names(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec!["canonical-model".to_string()])
    }
}

fn canonical_model() -> ModelWithPricing {
    ModelWithPricing {
        id: Uuid::new_v4(),
        model_name: "canonical-model".to_string(),
        model_display_name: "Canonical".to_string(),
        model_description: String::new(),
        model_icon: None,
        input_cost_per_token: 0,
        output_cost_per_token: 0,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        context_length: 4096,
        verifiable: true,
        aliases: vec!["alias-model".to_string()],
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

struct NoopMetricsService;

impl MetricsServiceTrait for NoopMetricsService {
    fn record_latency(&self, _name: &str, _duration: Duration, _tags: &[&str]) {}
    fn record_count(&self, _name: &str, _value: i64, _tags: &[&str]) {}
    fn record_histogram(&self, _name: &str, _value: f64, _tags: &[&str]) {}
}

struct NoopUsageRepository;

#[async_trait]
impl UsageRepository for NoopUsageRepository {
    async fn record_usage(&self, _request: RecordUsageDbRequest) -> anyhow::Result<UsageLogEntry> {
        anyhow::bail!("unused")
    }

    async fn get_balance(
        &self,
        _organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationBalanceInfo>> {
        Ok(None)
    }

    async fn get_usage_history(
        &self,
        _organization_id: Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        Ok((Vec::new(), 0))
    }

    async fn get_usage_history_by_api_key(
        &self,
        _api_key_id: Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        Ok((Vec::new(), 0))
    }

    async fn get_api_key_spend(&self, _api_key_id: Uuid) -> anyhow::Result<i64> {
        Ok(0)
    }

    async fn get_costs_by_inference_ids(
        &self,
        _organization_id: Uuid,
        _inference_ids: Vec<Uuid>,
    ) -> anyhow::Result<Vec<InferenceCost>> {
        Ok(Vec::new())
    }

    async fn get_stop_reason_by_response_id(
        &self,
        _response_id: Uuid,
    ) -> anyhow::Result<Option<StopReason>> {
        Ok(None)
    }

    async fn get_stop_reason_by_provider_request_id(
        &self,
        _provider_request_id: &str,
    ) -> anyhow::Result<Option<StopReason>> {
        Ok(None)
    }

    async fn get_usage_by_model(
        &self,
        _organization_id: Uuid,
        _start_date: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<Vec<UsageByModelEntry>> {
        Ok(Vec::new())
    }
}
