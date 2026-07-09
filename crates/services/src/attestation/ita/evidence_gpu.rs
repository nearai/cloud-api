use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::attestation::ita::{ItaNvgpuEvidence, ItaNvgpuEvidenceItem, ItaVerifierNonce};

use super::{derive_gpu_nonce, ItaEvidenceError};

#[derive(Deserialize)]
struct ProviderNvgpuPayload {
    gpu_nonce: Option<String>,
    nonce: Option<String>,
    arch: Option<String>,
    evidence_list: Option<Vec<ProviderNvgpuEvidenceItem>>,
}

#[derive(Deserialize)]
struct ProviderNvgpuEvidenceItem {
    certificate: Option<String>,
    evidence: Option<String>,
    firmware_version: Option<String>,
}

pub(super) fn build_nvgpu_evidence(
    model_attestations: &[Map<String, Value>],
    verifier_nonce: &ItaVerifierNonce,
) -> Result<ItaNvgpuEvidence, ItaEvidenceError> {
    let expected_gpu_nonce = derive_gpu_nonce(verifier_nonce)?;
    let mut arch: Option<String> = None;
    let mut evidence_list = Vec::new();
    for attestation in model_attestations {
        let payload = provider_nvgpu_payload(attestation)?;
        let observed_nonce = payload
            .gpu_nonce
            .or(payload.nonce)
            .ok_or(ItaEvidenceError::UnsupportedProviderEvidence)?;
        if !observed_nonce.eq_ignore_ascii_case(&expected_gpu_nonce) {
            return Err(ItaEvidenceError::GpuNonceMismatch);
        }
        let payload_arch = required_trimmed(payload.arch, "nvgpu.arch")?;
        match &arch {
            Some(existing) if existing != &payload_arch => {
                return Err(ItaEvidenceError::InconsistentGpuArch {
                    first: existing.clone(),
                    other: payload_arch,
                });
            }
            Some(_) => {}
            None => arch = Some(payload_arch),
        }
        let payload_items = payload
            .evidence_list
            .ok_or(ItaEvidenceError::MissingField("nvgpu.evidence_list"))?;
        if payload_items.is_empty() {
            return Err(ItaEvidenceError::MissingField("nvgpu.evidence_list"));
        }
        for item in payload_items {
            evidence_list.push(ita_nvgpu_item(item)?);
        }
    }
    let arch = arch.ok_or(ItaEvidenceError::UnsupportedProviderEvidence)?;
    Ok(ItaNvgpuEvidence {
        verifier_nonce: verifier_nonce.clone(),
        gpu_nonce: expected_gpu_nonce,
        arch,
        evidence_list,
    })
}

fn provider_nvgpu_payload(
    attestation: &Map<String, Value>,
) -> Result<ProviderNvgpuPayload, ItaEvidenceError> {
    if let Some(value) = attestation
        .get("ita_nvgpu")
        .or_else(|| attestation.get("nvgpu"))
    {
        return serde_json::from_value(value.clone()).map_err(|source| {
            ItaEvidenceError::MalformedProviderEvidence {
                field: "nvgpu",
                source,
            }
        });
    }
    let Some(value) = attestation.get("nvidia_payload") else {
        return Err(ItaEvidenceError::UnsupportedProviderEvidence);
    };
    let Some(raw_payload) = value.as_str() else {
        return Err(ItaEvidenceError::UnsupportedProviderEvidence);
    };
    serde_json::from_str(raw_payload).map_err(|source| {
        ItaEvidenceError::MalformedProviderEvidence {
            field: "nvidia_payload",
            source,
        }
    })
}

fn ita_nvgpu_item(
    item: ProviderNvgpuEvidenceItem,
) -> Result<ItaNvgpuEvidenceItem, ItaEvidenceError> {
    let certificate = required_trimmed(item.certificate, "nvgpu.evidence_list.certificate")?;
    let evidence = required_trimmed(item.evidence, "nvgpu.evidence_list.evidence")?;
    validate_base64("nvgpu.evidence_list.certificate", &certificate)?;
    validate_base64("nvgpu.evidence_list.evidence", &evidence)?;
    Ok(ItaNvgpuEvidenceItem {
        certificate,
        evidence,
        firmware_version: item.firmware_version,
    })
}

fn required_trimmed(
    value: Option<String>,
    field: &'static str,
) -> Result<String, ItaEvidenceError> {
    let trimmed = value
        .ok_or(ItaEvidenceError::MissingField(field))?
        .trim()
        .to_string();
    if trimmed.is_empty() {
        Err(ItaEvidenceError::MissingField(field))
    } else {
        Ok(trimmed)
    }
}

fn validate_base64(field: &'static str, value: &str) -> Result<(), ItaEvidenceError> {
    STANDARD
        .decode(value)
        .map(|_| ())
        .map_err(|source| ItaEvidenceError::InvalidBase64 { field, source })
}
