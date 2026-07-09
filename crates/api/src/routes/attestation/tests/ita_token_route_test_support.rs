use super::super::{get_ita_token, AttestationRouteState};
use async_trait::async_trait;
use axum::Router;
use config::{ItaPolicyIds, ItaTokenSigningAlg};
use services::{
    attestation::{
        ita::{
            ItaAttestationToken as ServiceItaAttestationToken,
            ItaAttestationType as ServiceItaAttestationType,
            ItaModelAliasResolved as ServiceItaModelAliasResolved,
            ItaModelToken as ServiceItaModelToken, ItaTokenQuery as ServiceItaTokenQuery,
            ItaTokenResponse as ServiceItaTokenResponse, ItaTokenType as ServiceItaTokenType,
        },
        AttestationError, SignatureLookupResult,
    },
    models::{ModelInfo, ModelWithPricing, ModelsError, ModelsServiceTrait},
};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const POLICY_A: &str = "11111111-1111-4111-8111-111111111111";
const POLICY_B: &str = "22222222-2222-4222-8222-222222222222";

#[derive(Clone)]
pub(super) struct RecordingItaAttestationService {
    result: Result<ServiceItaTokenResponse, AttestationError>,
    queries: Arc<Mutex<Vec<ServiceItaTokenQuery>>>,
}

impl RecordingItaAttestationService {
    pub(super) fn ok(response: ServiceItaTokenResponse) -> Self {
        Self {
            result: Ok(response),
            queries: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(super) fn err(error: AttestationError) -> Self {
        Self {
            result: Err(error),
            queries: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(super) fn queries(&self) -> Vec<ServiceItaTokenQuery> {
        self.queries
            .lock()
            .unwrap_or_else(|error| panic!("query lock poisoned: {error}"))
            .clone()
    }

    pub(super) fn only_query(&self) -> ServiceItaTokenQuery {
        let queries = self.queries();
        assert_eq!(queries.len(), 1);
        queries[0].clone()
    }
}

#[async_trait]
impl services::attestation::ports::AttestationServiceTrait for RecordingItaAttestationService {
    async fn get_chat_signature(
        &self,
        _chat_id: &str,
        _signing_algo: Option<String>,
    ) -> Result<SignatureLookupResult, AttestationError> {
        Err(AttestationError::InternalError("unused".to_string()))
    }

    async fn store_chat_signature_from_provider(
        &self,
        _chat_id: &str,
    ) -> Result<(), AttestationError> {
        Ok(())
    }

    async fn store_chat_signature(
        &self,
        _chat_id: &str,
        _request_hash: String,
        _response_hash: String,
    ) -> Result<(), AttestationError> {
        Ok(())
    }

    async fn store_response_signature(
        &self,
        _response_id: &str,
        _request_hash: String,
        _response_hash: String,
    ) -> Result<(), AttestationError> {
        Ok(())
    }

    async fn get_attestation_report(
        &self,
        _model: Option<String>,
        _signing_algo: Option<String>,
        _nonce: Option<String>,
        _signing_address: Option<String>,
        _include_tls_fingerprint: bool,
        _provider_filter: Option<inference_providers::ProviderTier>,
    ) -> Result<services::attestation::models::AttestationReport, AttestationError> {
        Err(AttestationError::InternalError("unused".to_string()))
    }

    async fn get_ita_attestation_token(
        &self,
        query: ServiceItaTokenQuery,
    ) -> Result<ServiceItaTokenResponse, AttestationError> {
        let mut queries = self
            .queries
            .lock()
            .map_err(|_| AttestationError::InternalError("query lock poisoned".to_string()))?;
        queries.push(query);
        self.result.clone()
    }

    async fn verify_vpc_signature(
        &self,
        _timestamp: i64,
        _signature: String,
    ) -> Result<bool, AttestationError> {
        Ok(false)
    }
}

#[derive(Clone, Default)]
pub(super) struct TestModelsService {
    alias: Option<(String, String)>,
}

impl TestModelsService {
    pub(super) fn with_alias(alias: &str, canonical: &str) -> Self {
        Self {
            alias: Some((alias.to_string(), canonical.to_string())),
        }
    }
}

#[async_trait]
impl ModelsServiceTrait for TestModelsService {
    async fn get_models(&self) -> Result<Vec<ModelInfo>, ModelsError> {
        Ok(Vec::new())
    }

    async fn get_models_with_pricing(&self) -> Result<Vec<ModelWithPricing>, ModelsError> {
        Ok(Vec::new())
    }

    async fn get_model_by_name(&self, model_name: &str) -> Result<ModelWithPricing, ModelsError> {
        Ok(model_with_name(model_name))
    }

    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> Result<ModelWithPricing, ModelsError> {
        let canonical = self
            .alias
            .as_ref()
            .filter(|(alias, _)| alias == identifier)
            .map(|(_, canonical)| canonical.as_str())
            .unwrap_or(identifier);
        Ok(model_with_name(canonical))
    }

    async fn resolve_alias_cached(&self, _identifier: &str) -> Option<String> {
        None
    }

    async fn get_configured_model_names(&self) -> Result<Vec<String>, ModelsError> {
        Ok(Vec::new())
    }

    async fn invalidate_models_cache(&self) {}
}

pub(super) fn public_ita_server(
    attestation_service: RecordingItaAttestationService,
    models_service: TestModelsService,
) -> axum_test::TestServer {
    let state = AttestationRouteState {
        attestation_service: Arc::new(attestation_service),
        models_service: Arc::new(models_service),
        ohttp_attestation: None,
    };
    let app = Router::new()
        .route("/attestation/ita-token", axum::routing::get(get_ita_token))
        .with_state(state);
    axum_test::TestServer::new(app)
}

pub(super) fn sample_ita_response(
    alias: Option<ServiceItaModelAliasResolved>,
) -> ServiceItaTokenResponse {
    ServiceItaTokenResponse {
        gateway: ServiceItaAttestationToken {
            token: "gateway.jwt".to_string(),
            token_type: ServiceItaTokenType::Jwt,
            attestation_type: ServiceItaAttestationType::Tdx,
            token_signing_alg: ItaTokenSigningAlg::Rs256,
            ita_request_id: Some("gateway-request".to_string()),
        },
        models: alias
            .as_ref()
            .map(|_| {
                vec![ServiceItaModelToken {
                    model: "canonical-model".to_string(),
                    attestation: ServiceItaAttestationToken {
                        token: "model.jwt".to_string(),
                        token_type: ServiceItaTokenType::Jwt,
                        attestation_type: ServiceItaAttestationType::Nvgpu,
                        token_signing_alg: ItaTokenSigningAlg::Rs256,
                        ita_request_id: Some("model-request".to_string()),
                    },
                }]
            })
            .unwrap_or_default(),
        jwks_url: "https://portal.example.test/certs".to_string(),
        policy_ids: ItaPolicyIds::parse_csv(&format!("{POLICY_A},{POLICY_B}"), "policy_ids")
            .unwrap_or_else(|error| panic!("sample policy IDs are valid: {error}")),
        policy_must_match: true,
        nonce: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
        model_alias_resolved: alias,
    }
}

fn model_with_name(model_name: &str) -> ModelWithPricing {
    ModelWithPricing {
        id: Uuid::new_v4(),
        model_name: model_name.to_string(),
        model_display_name: model_name.to_string(),
        model_description: String::new(),
        model_icon: None,
        input_cost_per_token: 0,
        output_cost_per_token: 0,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        context_length: 0,
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
