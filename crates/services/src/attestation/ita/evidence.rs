use base64::{engine::general_purpose::STANDARD, Engine as _};
use config::ItaEffectivePolicy;
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256, Sha512};

use crate::attestation::ita::{
    ItaAttestRequest, ItaTdxEvidence, ItaVerifierNonce, ItaVerifierNonceDecodeError,
};
use crate::attestation::models::DstackCpuQuote;

#[path = "evidence_gpu.rs"]
mod evidence_gpu;

use evidence_gpu::build_nvgpu_evidence;

pub struct ItaGatewayRuntimeDataInput<'a> {
    pub signing_algo: &'a str,
    pub signing_address: &'a str,
    pub caller_nonce: &'a str,
    pub tls_cert_fingerprint: Option<&'a str>,
}

pub struct ItaGatewayEvidenceInput<'a> {
    pub gateway: &'a DstackCpuQuote,
    pub verifier_nonce: &'a ItaVerifierNonce,
    pub policy: ItaEffectivePolicy,
}

pub struct ItaModelEvidenceInput<'a> {
    pub gateway: &'a DstackCpuQuote,
    pub model_attestations: &'a [Map<String, Value>],
    pub verifier_nonce: &'a ItaVerifierNonce,
    pub policy: ItaEffectivePolicy,
}

#[derive(Debug, thiserror::Error)]
pub enum ItaEvidenceError {
    #[error("missing ITA evidence field: {0}")]
    MissingField(&'static str),
    #[error("ITA evidence field '{field}' is not valid hex")]
    InvalidHex {
        field: &'static str,
        #[source]
        source: hex::FromHexError,
    },
    #[error("ITA evidence field '{field}' is not valid base64")]
    InvalidBase64 {
        field: &'static str,
        #[source]
        source: base64::DecodeError,
    },
    #[error("gateway TDX report_data does not match ITA nonce-bound runtime data")]
    ReportDataMismatch,
    #[error("provider evidence is not ITA-compatible")]
    UnsupportedProviderEvidence,
    #[error("provider GPU evidence is not bound to the ITA verifier nonce")]
    GpuNonceMismatch,
    #[error("provider GPU evidence mixes architectures ('{first}' vs '{other}')")]
    InconsistentGpuArch { first: String, other: String },
    #[error("provider evidence field '{field}' is malformed")]
    MalformedProviderEvidence {
        field: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("ITA verifier nonce is malformed")]
    MalformedVerifierNonce {
        #[source]
        source: ItaVerifierNonceDecodeError,
    },
}

#[derive(Serialize)]
struct RuntimeDataBinding<'a> {
    signing_algo: &'a str,
    signing_address: &'a str,
    caller_nonce: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tls_cert_fingerprint: Option<&'a str>,
}

pub fn build_gateway_attest_request(
    input: ItaGatewayEvidenceInput<'_>,
) -> Result<ItaAttestRequest, ItaEvidenceError> {
    let tdx = build_tdx_evidence(input.gateway, input.verifier_nonce)?;
    Ok(ItaAttestRequest {
        policy_ids: input.policy.policy_ids,
        token_signing_alg: input.policy.token_signing_alg,
        policy_must_match: input.policy.policy_must_match,
        tdx: Some(tdx),
        nvgpu: None,
    })
}

pub fn build_model_attest_request(
    input: ItaModelEvidenceInput<'_>,
) -> Result<ItaAttestRequest, ItaEvidenceError> {
    let tdx = build_tdx_evidence(input.gateway, input.verifier_nonce)?;
    let nvgpu = build_nvgpu_evidence(input.model_attestations, input.verifier_nonce)?;
    Ok(ItaAttestRequest {
        policy_ids: input.policy.policy_ids,
        token_signing_alg: input.policy.token_signing_alg,
        policy_must_match: input.policy.policy_must_match,
        tdx: Some(tdx),
        nvgpu: Some(nvgpu),
    })
}

fn build_tdx_evidence(
    gateway: &DstackCpuQuote,
    verifier_nonce: &ItaVerifierNonce,
) -> Result<ItaTdxEvidence, ItaEvidenceError> {
    let quote = decode_required_hex("tdx.quote", &gateway.intel_quote)?;
    let runtime_data = runtime_data_bytes(gateway)?;
    verify_report_data(gateway, verifier_nonce, &runtime_data)?;
    Ok(ItaTdxEvidence {
        quote: STANDARD.encode(quote),
        runtime_data: STANDARD.encode(runtime_data),
        // Dstack's JSON event log is not the raw CCEL/NEL format accepted by ITA.
        event_log: None,
        verifier_nonce: verifier_nonce.clone(),
    })
}

fn runtime_data_bytes(gateway: &DstackCpuQuote) -> Result<Vec<u8>, ItaEvidenceError> {
    build_gateway_runtime_data(ItaGatewayRuntimeDataInput {
        signing_algo: &gateway.signing_algo,
        signing_address: &gateway.signing_address,
        caller_nonce: &gateway.request_nonce,
        tls_cert_fingerprint: gateway.tls_cert_fingerprint.as_deref(),
    })
}

pub fn build_gateway_runtime_data(
    input: ItaGatewayRuntimeDataInput<'_>,
) -> Result<Vec<u8>, ItaEvidenceError> {
    let binding = RuntimeDataBinding {
        signing_algo: input.signing_algo,
        signing_address: input.signing_address,
        caller_nonce: input.caller_nonce,
        tls_cert_fingerprint: input.tls_cert_fingerprint,
    };
    serde_json::to_vec(&binding).map_err(|source| ItaEvidenceError::MalformedProviderEvidence {
        field: "tdx.runtime_data",
        source,
    })
}

fn verify_report_data(
    gateway: &DstackCpuQuote,
    verifier_nonce: &ItaVerifierNonce,
    runtime_data: &[u8],
) -> Result<(), ItaEvidenceError> {
    let report_data = decode_required_hex("tdx.report_data", &gateway.report_data)?;
    let expected = build_tdx_report_data(verifier_nonce, runtime_data)?;
    if report_data.as_slice() == expected.as_slice() {
        Ok(())
    } else {
        Err(ItaEvidenceError::ReportDataMismatch)
    }
}

pub fn build_tdx_report_data(
    verifier_nonce: &ItaVerifierNonce,
    runtime_data: &[u8],
) -> Result<Vec<u8>, ItaEvidenceError> {
    let mut hasher = Sha512::new();
    hasher.update(nonce_material(verifier_nonce)?);
    hasher.update(runtime_data);
    Ok(hasher.finalize().to_vec())
}

pub fn derive_gpu_nonce(verifier_nonce: &ItaVerifierNonce) -> Result<String, ItaEvidenceError> {
    let mut hasher = Sha256::new();
    hasher.update(nonce_material(verifier_nonce)?);
    Ok(hex::encode(hasher.finalize()))
}

pub(super) fn nonce_material(
    verifier_nonce: &ItaVerifierNonce,
) -> Result<Vec<u8>, ItaEvidenceError> {
    verifier_nonce
        .nonce_material()
        .map_err(|source| ItaEvidenceError::MalformedVerifierNonce { source })
}

fn decode_required_hex(field: &'static str, raw: &str) -> Result<Vec<u8>, ItaEvidenceError> {
    let trimmed = raw.trim();
    let hex_value = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    if hex_value.is_empty() {
        return Err(ItaEvidenceError::MissingField(field));
    }
    hex::decode(hex_value).map_err(|source| ItaEvidenceError::InvalidHex { field, source })
}

#[cfg(test)]
#[path = "evidence_test_support.rs"]
mod evidence_test_support;

#[cfg(test)]
#[path = "evidence_nvgpu_tests.rs"]
mod nvgpu_tests;

#[cfg(test)]
#[path = "evidence_tdx_tests.rs"]
mod tdx_tests;
