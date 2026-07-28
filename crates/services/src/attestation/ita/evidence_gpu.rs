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
    let certificate = normalize_certificate_chain("nvgpu.evidence_list.certificate", &certificate)?;
    validate_base64("nvgpu.evidence_list.evidence", &evidence)?;
    Ok(ItaNvgpuEvidenceItem {
        certificate,
        evidence,
        firmware_version: item.firmware_version,
    })
}

const PEM_CERTIFICATE_BEGIN: &str = "-----BEGIN CERTIFICATE-----";
const PEM_CERTIFICATE_END: &str = "-----END CERTIFICATE-----";
const PEM_LINE_WIDTH: usize = 64;

/// Re-encode the provider's GPU certificate chain as canonical PEM.
///
/// Providers read the chain out of NVML's fixed-size buffer, so the wire
/// value can carry trailing NUL padding after the last certificate. NVIDIA's
/// NRAS accepts that chain, but Intel Trust Authority rejects the whole
/// attest request with a bare 400 ("Failed to verify GPU evidence"). Intel's
/// own client re-encodes every certificate before submission, so mirror
/// that: keep each CERTIFICATE block's payload and drop everything else.
fn normalize_certificate_chain(
    field: &'static str,
    value: &str,
) -> Result<String, ItaEvidenceError> {
    let decoded = STANDARD
        .decode(value)
        .map_err(|source| ItaEvidenceError::InvalidBase64 { field, source })?;
    let text = String::from_utf8_lossy(&decoded);
    let mut canonical = String::new();
    let mut found_certificate = false;
    let mut rest: &str = &text;
    while let Some(begin) = rest.find(PEM_CERTIFICATE_BEGIN) {
        let after_begin = &rest[begin + PEM_CERTIFICATE_BEGIN.len()..];
        let Some(end) = after_begin.find(PEM_CERTIFICATE_END) else {
            return Err(ItaEvidenceError::InvalidPemChain { field });
        };
        let payload: String = after_begin[..end]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let der = STANDARD
            .decode(&payload)
            .map_err(|source| ItaEvidenceError::InvalidBase64 { field, source })?;
        if der.is_empty() {
            return Err(ItaEvidenceError::InvalidPemChain { field });
        }
        canonical.push_str(PEM_CERTIFICATE_BEGIN);
        canonical.push('\n');
        let encoded = STANDARD.encode(der);
        let mut offset = 0;
        while offset < encoded.len() {
            let line_end = (offset + PEM_LINE_WIDTH).min(encoded.len());
            canonical.push_str(&encoded[offset..line_end]);
            canonical.push('\n');
            offset = line_end;
        }
        canonical.push_str(PEM_CERTIFICATE_END);
        canonical.push('\n');
        found_certificate = true;
        rest = &after_begin[end + PEM_CERTIFICATE_END.len()..];
    }
    if !found_certificate {
        return Err(ItaEvidenceError::InvalidPemChain { field });
    }
    Ok(STANDARD.encode(canonical.as_bytes()))
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
