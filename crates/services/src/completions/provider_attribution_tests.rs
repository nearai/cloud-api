use super::*;
use crate::metrics::capturing::CapturingMetricsService;
use crate::models::ModelWithPricing;
use crate::test_utils::{CapturingUsageService, MockAttestationService};
use std::sync::Arc;
use std::time::Duration;

struct StaticModelsRepository {
    model: ModelWithPricing,
}

#[async_trait::async_trait]
impl ModelsRepository for StaticModelsRepository {
    async fn get_all_active_models(&self) -> Result<Vec<ModelWithPricing>, anyhow::Error> {
        Ok(vec![self.model.clone()])
    }

    async fn get_model_by_name(
        &self,
        model_name: &str,
    ) -> Result<Option<ModelWithPricing>, anyhow::Error> {
        Ok((model_name == self.model.model_name).then(|| self.model.clone()))
    }

    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> Result<Option<ModelWithPricing>, anyhow::Error> {
        Ok((identifier == self.model.model_name
            || self.model.aliases.iter().any(|alias| alias == identifier))
        .then(|| self.model.clone()))
    }

    async fn get_configured_model_names(&self) -> Result<Vec<String>, anyhow::Error> {
        Ok(vec![self.model.model_name.clone()])
    }
}

pub(super) struct StaticOrganizationLimitRepository;

#[async_trait::async_trait]
impl ports::OrganizationConcurrentLimitRepository for StaticOrganizationLimitRepository {
    async fn get_concurrent_limit(&self, _org_id: Uuid) -> Result<Option<u32>, anyhow::Error> {
        Ok(Some(DEFAULT_CONCURRENT_LIMIT))
    }
}

pub(super) fn test_model(model_name: &str) -> ModelWithPricing {
    ModelWithPricing {
        upcoming_pricing: None,
        id: Uuid::new_v4(),
        model_name: model_name.to_string(),
        model_display_name: model_name.to_string(),
        model_description: "test model".to_string(),
        model_icon: None,
        input_cost_per_token: 1,
        output_cost_per_token: 1,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        text_pricing: None,
        context_length: 4096,
        verifiable: true,
        aliases: Vec::new(),
        owned_by: "near".to_string(),
        provider_type: "vllm".to_string(),
        provider_config: None,
        attestation_supported: true,
        input_modalities: Some(vec!["text".to_string()]),
        output_modalities: Some(vec!["text".to_string()]),
        inference_url: Some("mock://near".to_string()),
        hugging_face_id: None,
        quantization: None,
        max_output_length: None,
        supported_sampling_parameters: Vec::new(),
        supported_features: Vec::new(),
        datacenters: None,
        is_ready: None,
        deprecation_date: None,
        successor_model_name: None,
        openrouter_slug: None,
        is_listed: true,
        created_at: chrono::Utc::now(),
    }
}

pub(super) fn completion_request(model: &str) -> ports::CompletionRequest {
    ports::CompletionRequest {
        request_priority: 0,
        session_hint: None,
        request_id: Uuid::new_v4(),
        model: model.to_string(),
        messages: vec![ports::CompletionMessage {
            reasoning_content: None,
            role: "user".to_string(),
            content: serde_json::Value::String("hello".to_string()),
            tool_call_id: None,
            tool_calls: None,
        }],
        max_tokens: None,
        temperature: None,
        top_p: None,
        stop: None,
        stream: Some(false),
        n: None,
        service_tier: None,
        user_id: crate::UserId(Uuid::new_v4()),
        api_key_id: Uuid::new_v4().to_string(),
        organization_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
        fallback_enabled: true,
        metadata: None,
        store: None,
        body_hash: "test-body-hash".to_string(),
        response_id: None,
        skip_provider_chat_signature: true,
        original_request: None,
        extra: std::collections::HashMap::new(),
    }
}

async fn wait_for_usage_requests(
    usage_service: &CapturingUsageService,
    expected: usize,
) -> Vec<RecordUsageServiceRequest> {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let requests = usage_service.get_requests();
            if requests.len() >= expected {
                return requests;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let actual = usage_service.get_requests().len();
        panic!("timed out waiting for {expected} usage requests, only {actual} recorded")
    })
}

async fn completion_service_with_mock_providers(
    model_name: &str,
    near_fails: bool,
    chutes_fails: bool,
) -> (CompletionServiceImpl, Arc<CapturingUsageService>) {
    use inference_providers::mock::{MockProvider, RequestMatcher, ResponseTemplate};
    use inference_providers::{CompletionError, ProviderSource, ProviderTier};

    let pool = Arc::new(InferenceProviderPool::new(
        None,
        config::ExternalProvidersConfig::default(),
    ));
    let near = Arc::new(
        MockProvider::new_accept_all()
            .with_tier(ProviderTier::Near)
            .with_provider_source(ProviderSource::Vllm),
    );
    if near_fails {
        near.set_error_override(Some(CompletionError::HttpError {
            status_code: 503,
            message: "near overloaded".to_string(),
            is_external: true,
        }))
        .await;
    } else {
        near.when(RequestMatcher::Any)
            .respond_with(ResponseTemplate::new("served-by-near-primary"))
            .await;
    }

    let chutes = Arc::new(
        MockProvider::new_accept_all()
            .with_tier(ProviderTier::Attested3p)
            .with_provider_source(ProviderSource::Chutes),
    );
    if chutes_fails {
        chutes
            .set_error_override(Some(CompletionError::HttpError {
                status_code: 503,
                message: "chutes overloaded".to_string(),
                is_external: true,
            }))
            .await;
    } else {
        chutes
            .when(RequestMatcher::Any)
            .respond_with(ResponseTemplate::new("served-by-chutes-fallback"))
            .await;
    }

    pool.register_provider(model_name.to_string(), near).await;
    pool.register_pinned_secondary_provider(model_name.to_string(), chutes, None)
        .await;

    let usage_service = Arc::new(CapturingUsageService::new());
    let service = CompletionServiceImpl::new(
        pool,
        Arc::new(MockAttestationService),
        usage_service.clone(),
        Arc::new(CapturingMetricsService::new()),
        Arc::new(StaticModelsRepository {
            model: test_model(model_name),
        }),
        Arc::new(StaticOrganizationLimitRepository),
    );
    (service, usage_service)
}

#[tokio::test]
async fn fallback_chutes_attribution_reaches_usage_request() {
    let model_name = "z-ai/glm-5.1";
    let (service, usage_service) =
        completion_service_with_mock_providers(model_name, true, false).await;

    let response = service
        .create_chat_completion(completion_request(model_name))
        .await
        .expect("fallback provider should serve the request");

    let requests = wait_for_usage_requests(&usage_service, 1).await;
    assert_eq!(requests.len(), 1, "expected one usage request");
    let attribution = requests[0].provider_attribution;
    assert!(!response.response.id.is_empty());
    assert_eq!(
        attribution.served_provider_tier,
        Some(crate::usage::ServedProviderTier::Attested3p)
    );
    assert_eq!(
        attribution.served_provider_type,
        Some(crate::usage::ServedProviderType::Chutes)
    );
    assert!(attribution.served_via_fallback);
}

#[tokio::test]
async fn primary_near_attribution_reaches_usage_request() {
    let model_name = "z-ai/glm-5.1";
    let (service, usage_service) =
        completion_service_with_mock_providers(model_name, false, false).await;

    let response = service
        .create_chat_completion(completion_request(model_name))
        .await
        .expect("primary NEAR provider should serve the request");

    let requests = wait_for_usage_requests(&usage_service, 1).await;
    assert_eq!(requests.len(), 1, "expected one usage request");
    let attribution = requests[0].provider_attribution;
    assert!(!response.response.id.is_empty());
    assert_eq!(
        attribution.served_provider_tier,
        Some(crate::usage::ServedProviderTier::Near)
    );
    assert_eq!(
        attribution.served_provider_type,
        Some(crate::usage::ServedProviderType::Vllm)
    );
    assert!(!attribution.served_via_fallback);
}

#[tokio::test]
async fn failed_providers_do_not_record_served_attribution() {
    let model_name = "z-ai/glm-5.1";
    let (service, usage_service) =
        completion_service_with_mock_providers(model_name, true, true).await;

    let result = service
        .create_chat_completion(completion_request(model_name))
        .await;

    assert!(result.is_err(), "all providers should fail the request");
    let requests = usage_service.get_requests();
    assert!(
        requests.is_empty(),
        "terminal provider failures must not record successful usage"
    );
}

async fn estimate_service(
    model_name: &str,
    tier: inference_providers::ProviderTier,
    source: inference_providers::ProviderSource,
    attestation_supported: bool,
) -> (CompletionServiceImpl, Arc<CapturingUsageService>) {
    use inference_providers::mock::{MockProvider, RequestMatcher, ResponseTemplate};

    let pool = Arc::new(InferenceProviderPool::new(
        None,
        config::ExternalProvidersConfig::default(),
    ));
    let provider = Arc::new(
        MockProvider::new_accept_all()
            .with_tier(tier)
            .with_provider_source(source),
    );
    provider
        .when(RequestMatcher::Any)
        .respond_with(
            ResponseTemplate::new("hello from the model, streamed without a usage summary")
                .with_usage_omitted(),
        )
        .await;
    pool.register_provider(model_name.to_string(), provider)
        .await;

    let mut model = test_model(model_name);
    model.attestation_supported = attestation_supported;
    let usage_service = Arc::new(CapturingUsageService::new());
    let service = CompletionServiceImpl::new(
        pool,
        Arc::new(MockAttestationService),
        usage_service.clone(),
        Arc::new(CapturingMetricsService::new()),
        Arc::new(StaticModelsRepository { model }),
        Arc::new(StaticOrganizationLimitRepository),
    );
    (service, usage_service)
}

async fn drain_stream(service: &CompletionServiceImpl, model_name: &str, header: Option<&str>) {
    use futures::StreamExt;

    let mut request = completion_request(model_name);
    request.stream = Some(true);
    if let Some(key) = header {
        request
            .extra
            .insert(key.to_string(), serde_json::json!("value"));
    }
    let stream = service
        .create_chat_completion_stream(request)
        .await
        .expect("the stream should start");
    let _ = stream.collect::<Vec<_>>().await;
}

#[tokio::test]
async fn plaintext_request_on_the_near_fleet_records_an_estimate() {
    use inference_providers::{ProviderSource, ProviderTier};

    let model_name = "z-ai/glm-5.1";
    let (service, usage_service) =
        estimate_service(model_name, ProviderTier::Near, ProviderSource::Vllm, true).await;

    drain_stream(&service, model_name, None).await;

    let requests = wait_for_usage_requests(&usage_service, 1).await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0].input_tokens > 0 && requests[0].output_tokens > 0);
    assert_eq!(
        requests[0].stop_reason,
        Some(crate::usage::StopReason::Incomplete)
    );
}

#[tokio::test]
async fn encrypted_request_on_the_near_fleet_records_no_estimate() {
    use inference_providers::{ProviderSource, ProviderTier};

    let model_name = "z-ai/glm-5.1";
    let (service, usage_service) =
        estimate_service(model_name, ProviderTier::Near, ProviderSource::Vllm, true).await;

    drain_stream(
        &service,
        model_name,
        Some(crate::common::encryption_headers::CLIENT_PUB_KEY),
    )
    .await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        usage_service.get_requests().is_empty(),
        "byte counts over ciphertext must not be billed"
    );
}

#[tokio::test]
async fn encryption_headers_do_not_turn_off_estimates_on_other_providers() {
    use inference_providers::{ProviderSource, ProviderTier};

    let cases = [
        (
            ProviderTier::NonAttested,
            ProviderSource::External,
            false,
            crate::common::encryption_headers::ENCRYPTION_VERSION,
        ),
        (
            ProviderTier::Attested3p,
            ProviderSource::Chutes,
            true,
            crate::common::encryption_headers::CLIENT_PUB_KEY,
        ),
        (
            ProviderTier::Attested3p,
            ProviderSource::Tinfoil,
            true,
            crate::common::encryption_headers::SIGNING_ALGO,
        ),
    ];
    for (tier, source, attested, header) in cases {
        let model_name = "z-ai/glm-5.1";
        let (service, usage_service) = estimate_service(model_name, tier, source, attested).await;

        drain_stream(&service, model_name, Some(header)).await;

        let requests = wait_for_usage_requests(&usage_service, 1).await;
        assert_eq!(requests.len(), 1, "{source:?} with {header}");
        assert!(
            requests[0].output_tokens > 0,
            "{source:?} with {header} must still be estimated"
        );
    }
}
