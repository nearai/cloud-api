use serde::{Deserialize, Serialize};
use services::attestation::ita::{
    ItaAttestationToken as ServiceItaAttestationToken,
    ItaAttestationType as ServiceItaAttestationType, ItaTokenResponse as ServiceItaTokenResponse,
    ItaTokenType as ServiceItaTokenType,
};
use utoipa::{IntoParams, ToSchema};

/// Query parameters for Intel Trust Authority token attestation.
#[derive(Debug, Serialize, Deserialize, ToSchema, IntoParams)]
pub struct ItaTokenQuery {
    pub model: Option<String>,
    pub nonce: Option<String>,
    pub signing_algo: Option<String>,
    pub signing_address: Option<String>,
    pub include_tls_fingerprint: Option<String>,
    pub policy_ids: Option<String>,
    pub policy_must_match: Option<String>,
    pub token_signing_alg: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ItaTokenItem {
    pub token: String,
    pub token_type: String,
    pub attestation_type: String,
    pub token_signing_alg: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ita_request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ItaModelTokenItem {
    pub model: String,
    pub token: String,
    pub token_type: String,
    pub attestation_type: String,
    pub token_signing_alg: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ita_request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ItaModelAliasResolved {
    pub requested: String,
    pub canonical: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ItaTokenResponse {
    pub gateway: ItaTokenItem,
    pub models: Vec<ItaModelTokenItem>,
    pub jwks_url: String,
    pub policy_ids: Vec<String>,
    pub policy_must_match: bool,
    pub nonce: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_alias_resolved: Option<ItaModelAliasResolved>,
}

impl From<ServiceItaTokenResponse> for ItaTokenResponse {
    fn from(response: ServiceItaTokenResponse) -> Self {
        Self {
            gateway: response.gateway.into(),
            models: response
                .models
                .into_iter()
                .map(|model| {
                    let attestation = ItaTokenItem::from(model.attestation);
                    ItaModelTokenItem {
                        model: model.model,
                        token: attestation.token,
                        token_type: attestation.token_type,
                        attestation_type: attestation.attestation_type,
                        token_signing_alg: attestation.token_signing_alg,
                        ita_request_id: attestation.ita_request_id,
                    }
                })
                .collect(),
            jwks_url: response.jwks_url,
            policy_ids: response.policy_ids.to_strings(),
            policy_must_match: response.policy_must_match,
            nonce: response.nonce,
            model_alias_resolved: response.model_alias_resolved.map(|alias| {
                ItaModelAliasResolved {
                    requested: alias.requested,
                    canonical: alias.canonical,
                }
            }),
        }
    }
}

impl From<ServiceItaAttestationToken> for ItaTokenItem {
    fn from(token: ServiceItaAttestationToken) -> Self {
        Self {
            token: token.token,
            token_type: ita_token_type(token.token_type).to_string(),
            attestation_type: ita_attestation_type(token.attestation_type).to_string(),
            token_signing_alg: token.token_signing_alg.to_string(),
            ita_request_id: token.ita_request_id,
        }
    }
}

fn ita_token_type(token_type: ServiceItaTokenType) -> &'static str {
    match token_type {
        ServiceItaTokenType::Jwt => "JWT",
    }
}

fn ita_attestation_type(attestation_type: ServiceItaAttestationType) -> &'static str {
    match attestation_type {
        ServiceItaAttestationType::Tdx => "tdx",
        ServiceItaAttestationType::Nvgpu => "nvgpu",
    }
}
