use base64::{engine::general_purpose::STANDARD, Engine as _};
use config::{ItaPolicyIds, ItaPolicyOverride, ItaTokenSigningAlg};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItaGatewaySigningAlg {
    Ed25519,
    Ecdsa,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItaTokenType {
    #[serde(rename = "JWT")]
    Jwt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItaAttestationType {
    Tdx,
    Nvgpu,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaTokenQuery {
    pub model: Option<String>,
    pub nonce: Option<String>,
    pub signing_algo: Option<ItaGatewaySigningAlg>,
    pub signing_address: Option<String>,
    pub include_tls_fingerprint: Option<bool>,
    pub policy_ids: Option<ItaPolicyIds>,
    pub policy_must_match: Option<bool>,
    pub token_signing_alg: Option<ItaTokenSigningAlg>,
}

impl ItaTokenQuery {
    pub fn policy_override(&self) -> ItaPolicyOverride {
        ItaPolicyOverride {
            policy_ids: self.policy_ids.clone(),
            policy_must_match: self.policy_must_match,
            token_signing_alg: self.token_signing_alg,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaAttestationToken {
    pub token: String,
    pub token_type: ItaTokenType,
    pub attestation_type: ItaAttestationType,
    pub token_signing_alg: ItaTokenSigningAlg,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ita_request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaModelToken {
    pub model: String,
    #[serde(flatten)]
    pub attestation: ItaAttestationToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaModelAliasResolved {
    pub requested: String,
    pub canonical: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaTokenResponse {
    pub gateway: ItaAttestationToken,
    pub models: Vec<ItaModelToken>,
    pub jwks_url: String,
    pub policy_ids: ItaPolicyIds,
    pub policy_must_match: bool,
    pub nonce: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_alias_resolved: Option<ItaModelAliasResolved>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaVerifierNonce {
    pub val: String,
    pub iat: String,
    pub signature: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ItaVerifierNonceDecodeError {
    #[error("ITA verifier nonce field '{field}' is not valid base64")]
    InvalidBase64 {
        field: &'static str,
        #[source]
        source: base64::DecodeError,
    },
}

impl ItaVerifierNonce {
    pub fn nonce_material(&self) -> Result<Vec<u8>, ItaVerifierNonceDecodeError> {
        let val = decode_nonce_field("val", &self.val)?;
        let iat = decode_nonce_field("iat", &self.iat)?;
        let mut nonce_material = Vec::with_capacity(val.len() + iat.len());
        nonce_material.extend_from_slice(&val);
        nonce_material.extend_from_slice(&iat);
        Ok(nonce_material)
    }

    pub fn validate_wire_encoding(&self) -> Result<(), ItaVerifierNonceDecodeError> {
        decode_nonce_field("val", &self.val)?;
        decode_nonce_field("iat", &self.iat)?;
        decode_nonce_field("signature", &self.signature)?;
        Ok(())
    }
}

fn decode_nonce_field(
    field: &'static str,
    value: &str,
) -> Result<Vec<u8>, ItaVerifierNonceDecodeError> {
    STANDARD
        .decode(value)
        .map_err(|source| ItaVerifierNonceDecodeError::InvalidBase64 { field, source })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaNonceResponse {
    pub nonce: ItaVerifierNonce,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaTdxEvidence {
    pub quote: String,
    pub runtime_data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_log: Option<String>,
    pub verifier_nonce: ItaVerifierNonce,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaNvgpuEvidenceItem {
    pub certificate: String,
    pub evidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaNvgpuEvidence {
    pub verifier_nonce: ItaVerifierNonce,
    pub gpu_nonce: String,
    pub arch: String,
    pub evidence_list: Vec<ItaNvgpuEvidenceItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaAttestRequest {
    #[serde(skip_serializing_if = "ItaPolicyIds::is_empty")]
    pub policy_ids: ItaPolicyIds,
    pub token_signing_alg: ItaTokenSigningAlg,
    pub policy_must_match: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tdx: Option<ItaTdxEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nvgpu: Option<ItaNvgpuEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaAttestResponse {
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
