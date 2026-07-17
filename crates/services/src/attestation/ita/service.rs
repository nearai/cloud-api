use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use super::client_error_mapping::map_ita_client_error;
use super::{
    build_gateway_attest_request, build_gateway_runtime_data, build_model_attest_request,
    build_tdx_report_data, derive_gpu_nonce, ItaAttestRequest, ItaAttestationToken,
    ItaAttestationType, ItaGatewayEvidenceInput, ItaGatewayRuntimeDataInput, ItaGatewaySigningAlg,
    ItaModelAliasResolved, ItaModelEvidenceInput, ItaModelToken, ItaTokenQuery, ItaTokenResponse,
    ItaTokenType, ItaVerifierNonce,
};
use crate::{
    attestation::{
        decode_nonce_hex, generate_nonce_hex, AttestationError, AttestationService,
        GatewayQuoteInput,
    },
    inference_provider_pool::InferenceProviderPool,
};

pub struct ModelAttestationInput {
    pub model: String,
    pub signing_algo: Option<String>,
    pub nonce: Option<String>,
    pub signing_address: Option<String>,
    pub include_tls_fingerprint: bool,
}

#[async_trait]
pub trait ModelAttestationCollector: Send + Sync {
    async fn collect_model_attestations(
        &self,
        input: ModelAttestationInput,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, AttestationError>;
}

pub(in crate::attestation) struct ProviderPoolModelAttestationCollector {
    inference_provider_pool: Arc<InferenceProviderPool>,
}

impl ProviderPoolModelAttestationCollector {
    pub(in crate::attestation) fn new(inference_provider_pool: Arc<InferenceProviderPool>) -> Self {
        Self {
            inference_provider_pool,
        }
    }
}

#[async_trait]
impl ModelAttestationCollector for ProviderPoolModelAttestationCollector {
    async fn collect_model_attestations(
        &self,
        input: ModelAttestationInput,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, AttestationError> {
        self.inference_provider_pool
            .get_attestation_report(
                input.model,
                input.signing_algo,
                input.nonce,
                input.signing_address,
                input.include_tls_fingerprint,
                None,
            )
            .await
            .map_err(|e| AttestationError::ProviderError(e.to_string()))
    }
}

struct ItaModelPlan {
    canonical_model: String,
    model_alias_resolved: Option<ItaModelAliasResolved>,
    attest_request: ItaAttestRequest,
}

impl AttestationService {
    pub(in crate::attestation) async fn create_ita_attestation_token(
        &self,
        query: ItaTokenQuery,
    ) -> Result<ItaTokenResponse, AttestationError> {
        if !self.ita_config.enabled {
            return Err(AttestationError::ItaUnavailable {
                reason: "ITA attestation is disabled".to_string(),
            });
        }
        let client = self
            .ita_client
            .as_ref()
            .ok_or_else(|| AttestationError::ItaUnavailable {
                reason: "ITA client is not configured".to_string(),
            })?;

        let caller_nonce = query.nonce.clone().unwrap_or_else(generate_nonce_hex);
        decode_nonce_hex(&caller_nonce)?;
        let signing_algo = ita_gateway_signing_algo(query.signing_algo).to_string();
        let include_tls_fingerprint = query.include_tls_fingerprint.unwrap_or(false);
        let effective_policy = self.ita_config.effective_policy(&query.policy_override());

        let nonce_response = client
            .get_nonce(&Uuid::new_v4().to_string())
            .await
            .map_err(map_ita_client_error)?;
        let verifier_nonce = nonce_response.nonce;

        let gateway_attestation = self
            .collect_ita_gateway_quote(
                &verifier_nonce,
                &signing_algo,
                &caller_nonce,
                include_tls_fingerprint,
            )
            .await?;
        let gateway_request = build_gateway_attest_request(ItaGatewayEvidenceInput {
            gateway: &gateway_attestation,
            verifier_nonce: &verifier_nonce,
            policy: effective_policy.clone(),
        })
        .map_err(map_ita_evidence_error)?;

        let model_plan = self
            .prepare_ita_model_plan(&query, &verifier_nonce, &gateway_attestation, &signing_algo)
            .await?;
        let gateway_attest_response = client
            .attest(&Uuid::new_v4().to_string(), &gateway_request)
            .await
            .map_err(map_ita_client_error)?;
        let gateway = ItaAttestationToken {
            token: gateway_attest_response.token,
            token_type: ItaTokenType::Jwt,
            attestation_type: ItaAttestationType::Tdx,
            token_signing_alg: effective_policy.token_signing_alg,
            ita_request_id: gateway_attest_response.request_id,
        };

        let mut models = Vec::new();
        let mut model_alias_resolved = None;
        if let Some(plan) = model_plan {
            let model_attest_response = client
                .attest(&Uuid::new_v4().to_string(), &plan.attest_request)
                .await
                .map_err(map_ita_client_error)?;
            models.push(ItaModelToken {
                model: plan.canonical_model,
                attestation: ItaAttestationToken {
                    token: model_attest_response.token,
                    token_type: ItaTokenType::Jwt,
                    attestation_type: ItaAttestationType::Nvgpu,
                    token_signing_alg: effective_policy.token_signing_alg,
                    ita_request_id: model_attest_response.request_id,
                },
            });
            model_alias_resolved = plan.model_alias_resolved;
        }

        Ok(ItaTokenResponse {
            gateway,
            models,
            jwks_url: format!("{}/certs", self.ita_config.portal_base_url.as_str()),
            policy_ids: effective_policy.policy_ids,
            policy_must_match: effective_policy.policy_must_match,
            nonce: caller_nonce,
            model_alias_resolved,
        })
    }

    async fn collect_ita_gateway_quote(
        &self,
        verifier_nonce: &ItaVerifierNonce,
        signing_algo: &str,
        caller_nonce: &str,
        include_tls_fingerprint: bool,
    ) -> Result<super::super::models::DstackCpuQuote, AttestationError> {
        let signing_address = self.get_signing_address_hex(signing_algo)?;
        let tls_fingerprint = if include_tls_fingerprint {
            Some(self.tls_cert_fingerprint.clone().ok_or_else(|| {
                AttestationError::InternalError(
                    "include_tls_fingerprint=true but TLS_CERT_PATH is not set or fingerprint could not be computed".to_string(),
                )
            })?)
        } else {
            None
        };
        let runtime_data = build_gateway_runtime_data(ItaGatewayRuntimeDataInput {
            signing_algo,
            signing_address: &signing_address,
            caller_nonce,
            tls_cert_fingerprint: tls_fingerprint.as_deref(),
        })
        .map_err(map_ita_evidence_error)?;
        let report_data =
            build_tdx_report_data(verifier_nonce, &runtime_data).map_err(map_ita_evidence_error)?;

        self.gateway_quote_collector
            .collect_gateway_quote(GatewayQuoteInput {
                signing_address,
                signing_algo: signing_algo.to_string(),
                report_data,
                request_nonce: caller_nonce.to_string(),
                vpc: self.vpc_info.clone(),
                tls_cert_fingerprint: tls_fingerprint,
            })
            .await
    }

    async fn prepare_ita_model_plan(
        &self,
        query: &ItaTokenQuery,
        verifier_nonce: &ItaVerifierNonce,
        gateway_attestation: &super::super::models::DstackCpuQuote,
        signing_algo: &str,
    ) -> Result<Option<ItaModelPlan>, AttestationError> {
        let Some(requested_model) = query.model.as_ref() else {
            return Ok(None);
        };
        let resolved_model = self
            .models_repository
            .resolve_and_get_model(requested_model)
            .await
            .map_err(|e| AttestationError::ProviderError(format!("Failed to resolve model: {e}")))?
            .ok_or_else(|| {
                AttestationError::ProviderError(format!(
                    "Model '{requested_model}' not found. It's not a valid model name or alias."
                ))
            })?;
        let canonical_model = resolved_model.model_name;
        let model_alias_resolved =
            (canonical_model != *requested_model).then(|| ItaModelAliasResolved {
                requested: requested_model.clone(),
                canonical: canonical_model.clone(),
            });
        let gpu_nonce = derive_gpu_nonce(verifier_nonce).map_err(map_ita_evidence_error)?;
        let model_attestations = self
            .model_attestation_collector
            .collect_model_attestations(ModelAttestationInput {
                model: canonical_model.clone(),
                signing_algo: Some(signing_algo.to_string()),
                nonce: Some(gpu_nonce),
                signing_address: query.signing_address.clone(),
                include_tls_fingerprint: query.include_tls_fingerprint.unwrap_or(false),
            })
            .await?;
        let attest_request = build_model_attest_request(ItaModelEvidenceInput {
            gateway: gateway_attestation,
            model_attestations: &model_attestations,
            verifier_nonce,
            policy: self.ita_config.effective_policy(&query.policy_override()),
        })
        .map_err(map_ita_evidence_error)?;
        Ok(Some(ItaModelPlan {
            canonical_model,
            model_alias_resolved,
            attest_request,
        }))
    }
}

fn ita_gateway_signing_algo(algo: Option<ItaGatewaySigningAlg>) -> &'static str {
    match algo.unwrap_or(ItaGatewaySigningAlg::Ed25519) {
        ItaGatewaySigningAlg::Ed25519 => "ed25519",
        ItaGatewaySigningAlg::Ecdsa => "ecdsa",
    }
}

fn map_ita_evidence_error(error: impl std::fmt::Display) -> AttestationError {
    AttestationError::ItaInvalidEvidence {
        reason: error.to_string(),
    }
}
