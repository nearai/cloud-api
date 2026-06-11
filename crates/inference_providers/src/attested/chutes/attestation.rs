//! Transform Chutes [`InstanceEvidence`] into NEAR's attestation-report shape.
//!
//! The goal of the wider effort is one shared verification path. The pieces that
//! are *format-identical* to NEAR are normalized here so the shared verifier can
//! consume them:
//!
//! | NEAR field            | Source                                            |
//! |-----------------------|---------------------------------------------------|
//! | `intel_quote` (hex)   | base64 `quote` → raw bytes → hex                  |
//! | `tls_cert_fingerprint`| `SHA256(SPKI(certificate))` = `report_data[32:64]`|
//! | `nvidia_payload`      | `{arch, evidence_list:[{certificate,evidence}]}`  |
//! | `request_nonce`       | the caller's per-request nonce (passthrough)      |
//! | `instance_id`         | passthrough                                       |
//!
//! What is **deliberately not** produced here, because Chutes' bindings differ
//! from NEAR's and must be checked by the Chutes-specific verifier (next PR), not
//! NEAR's default verifier:
//!
//! - **`report_data[0:32]` freshness** = `SHA256(nonce ‖ instance_e2e_pubkey)`,
//!   where the ML-KEM-768 `e2e_pubkey` comes from `GET /e2e/instances/{chute}`
//!   (not in `/evidence`). The verifier fetches it and checks the binding.
//! - **`signing_address`** — Chutes has no separate signing address; identity is
//!   the cert SPKI bound in `report_data[32:64]` (above) and the attested
//!   ML-KEM key. NEAR's `StrictBoundReportDataVerifier` is therefore *not* the
//!   right verifier for Chutes; a `ChutesReportDataVerifier` is.
//! - **GPU nonce** — the NVIDIA evidence is bound to a Chutes-derived nonce, not
//!   the raw caller nonce, so the GPU-nonce check is Chutes-specific.
//! - **`event_log`** — Chutes ships no dstack-style RTMR3 event log, so
//!   measurement verification is register-pin (MRTD/RTMR), not replay.
//!
//! This module is a pure, fail-closed transform: malformed/absent required
//! fields produce an error, never a silently-incomplete report.

use serde_json::{json, Map, Value};

use super::evidence::InstanceEvidence;
use crate::spki_verifier::compute_spki_fingerprint_from_der;

/// Errors from transforming Chutes evidence into NEAR's report shape.
#[derive(Debug, thiserror::Error)]
pub enum TransformError {
    #[error("evidence field '{0}' is empty")]
    Empty(&'static str),
    #[error("evidence field '{field}' is not valid base64: {source}")]
    InvalidBase64 {
        field: &'static str,
        #[source]
        source: base64::DecodeError,
    },
    #[error("instance certificate is not a valid X.509 DER: {0}")]
    InvalidCertificate(String),
    #[error("instance has no GPU evidence (a confidential-GPU provider must present it)")]
    NoGpuEvidence,
    #[error("GPU evidence has an empty 'arch'")]
    EmptyGpuArch,
    #[error("GPU evidence mixes architectures ('{first}' vs '{other}')")]
    InconsistentGpuArch { first: String, other: String },
}

fn decode_b64(field: &'static str, s: &str) -> Result<Vec<u8>, TransformError> {
    use base64::Engine;
    // Trim surrounding whitespace/newlines — common in API/PEM-adjacent strings.
    let s = s.trim();
    if s.is_empty() {
        return Err(TransformError::Empty(field));
    }
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|source| TransformError::InvalidBase64 { field, source })
}

/// Hex-encode the instance's TDX quote (Chutes returns base64; NEAR's
/// `intel_quote` is hex).
pub fn intel_quote_hex(instance: &InstanceEvidence) -> Result<String, TransformError> {
    Ok(hex::encode(decode_b64("quote", &instance.quote)?))
}

/// `SHA256(SPKI(certificate))` — the value bound into `report_data[32:64]`.
pub fn tls_cert_fingerprint(instance: &InstanceEvidence) -> Result<String, TransformError> {
    let der = decode_b64("certificate", &instance.certificate)?;
    compute_spki_fingerprint_from_der(&der).map_err(TransformError::InvalidCertificate)
}

/// The instance certificate as raw DER (base64-decoded). The Chutes verifier
/// needs the DER to check `report_data[32:64] == SHA256(SPKI(cert))`.
pub fn certificate_der(instance: &InstanceEvidence) -> Result<Vec<u8>, TransformError> {
    decode_b64("certificate", &instance.certificate)
}

/// Assemble the NVIDIA NRAS-submittable payload from the per-GPU evidence.
///
/// The nonce the NVIDIA evidence is bound to is Chutes-derived (not the raw
/// caller nonce), so it is intentionally **not** set here — the Chutes verifier
/// supplies/checks it. This produces the `arch` + `evidence_list` shape NRAS
/// expects.
pub fn nvidia_payload(instance: &InstanceEvidence) -> Result<Value, TransformError> {
    if instance.gpu_evidence.is_empty() {
        return Err(TransformError::NoGpuEvidence);
    }
    // The single `arch` describes the whole evidence list, so all GPUs must
    // agree and it must be non-empty — fail closed otherwise rather than
    // silently labelling the payload with the first GPU's arch.
    let arch = instance.gpu_evidence[0].arch.trim();
    if arch.is_empty() {
        return Err(TransformError::EmptyGpuArch);
    }
    if let Some(g) = instance.gpu_evidence.iter().find(|g| g.arch.trim() != arch) {
        return Err(TransformError::InconsistentGpuArch {
            first: arch.to_string(),
            other: g.arch.trim().to_string(),
        });
    }
    // Validate each entry decodes (fail-closed) and emit the SAME (trimmed)
    // bytes we validated — otherwise whitespace-padded base64 would pass local
    // validation but NRAS would receive different bytes, failing far from the
    // root cause.
    let mut evidence_list = Vec::with_capacity(instance.gpu_evidence.len());
    for g in &instance.gpu_evidence {
        let certificate = g.certificate.trim();
        let evidence = g.evidence.trim();
        decode_b64("gpu_evidence.certificate", certificate)?;
        decode_b64("gpu_evidence.evidence", evidence)?;
        evidence_list.push(json!({ "certificate": certificate, "evidence": evidence }));
    }
    Ok(json!({ "arch": arch, "evidence_list": evidence_list }))
}

/// Normalize one Chutes instance's evidence into the **subset** of NEAR
/// attestation-report fields that are format-identical (`intel_quote`,
/// `tls_cert_fingerprint`, `nvidia_payload`, `request_nonce`, `instance_id`).
///
/// This is intentionally **not** a complete NEAR attestation report: NEAR's
/// `signing_address` and `event_log` are N/A for Chutes (no separate signing
/// address; register-pin instead of an event log), and `report_data` is not a
/// map field at all — it lives inside the decoded `intel_quote`. The Chutes
/// verifier (next PR) consumes this partial map and additionally checks the
/// Chutes-shaped bindings (`report_data[0:32] = SHA256(nonce ‖ e2e_pubkey)`,
/// `[32:64] = tls_cert_fingerprint`, GPU nonce, register-pin measurement).
///
/// **Do not route this map through NEAR's `verify_attestation_report`.** It
/// carries a `request_nonce` field, which NEAR's verifier compares against
/// `report_data[32:64]` — but Chutes seals `SHA256(nonce ‖ e2e_pubkey)` there,
/// so that path would fail confusingly. The Chutes verifier
/// (`services::attestation::chutes`) deliberately consumes the individual
/// helpers (`intel_quote_hex`, `nvidia_payload`, `certificate_der`) and the
/// derived GPU nonce instead; this whole-map form exists only for tests /
/// documentation of the shared-shape fields.
///
/// Fail-closed: any malformed/absent required field errors.
pub fn to_near_report(
    instance: &InstanceEvidence,
    request_nonce: &str,
) -> Result<Map<String, Value>, TransformError> {
    let mut m = Map::new();
    m.insert("intel_quote".into(), json!(intel_quote_hex(instance)?));
    m.insert(
        "tls_cert_fingerprint".into(),
        json!(tls_cert_fingerprint(instance)?),
    );
    m.insert(
        "nvidia_payload".into(),
        json!(nvidia_payload(instance)?.to_string()),
    );
    m.insert("request_nonce".into(), json!(request_nonce));
    m.insert("instance_id".into(), json!(instance.instance_id));
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::super::evidence::{GpuEvidence, InstanceEvidence};
    use super::*;

    fn gpu() -> GpuEvidence {
        GpuEvidence {
            certificate: "Y2VydA==".into(), // "cert"
            evidence: "ZXZpZA==".into(),    // "evid"
            arch: "HOPPER".into(),
        }
    }

    // A SYNTHETIC self-signed X.509 cert (DER, base64) — exercises the SPKI
    // fingerprint path without committing a real node's certificate (which
    // embeds hostnames/IPs into repo history). The real Chutes cert is exercised
    // by the live integration test in the verifier PR.
    const TEST_CERT_B64: &str = include_str!("testdata/synthetic_cert.b64");

    #[test]
    fn intel_quote_base64_to_hex() {
        let inst = InstanceEvidence {
            quote: "BAACAIE=".into(), // bytes 04 00 02 00 81
            gpu_evidence: vec![gpu()],
            instance_id: "i1".into(),
            certificate: TEST_CERT_B64.trim().into(),
        };
        assert_eq!(intel_quote_hex(&inst).unwrap(), "0400020081");
    }

    #[test]
    fn tls_fingerprint_from_synthetic_cert_matches_expected() {
        let inst = InstanceEvidence {
            quote: "BAACAIE=".into(),
            gpu_evidence: vec![gpu()],
            instance_id: "i1".into(),
            certificate: TEST_CERT_B64.trim().into(),
        };
        let fp = tls_cert_fingerprint(&inst).expect("synthetic cert SPKI fingerprint");
        // Pin the exact value: a length-only check would miss a regression in
        // compute_spki_fingerprint_from_der (e.g. hashing the whole cert DER
        // instead of the SPKI), which would only surface later as a runtime
        // report_data[32:64] mismatch.
        assert_eq!(
            fp,
            "e7c25815d0d940fea893d56984e131788afa6e931920093c9c2896fb04dea0da"
        );
    }

    #[test]
    fn nvidia_payload_assembles_evidence_list() {
        let inst = InstanceEvidence {
            quote: "BAACAIE=".into(),
            gpu_evidence: vec![gpu(), gpu()],
            instance_id: "i1".into(),
            certificate: TEST_CERT_B64.trim().into(),
        };
        let p = nvidia_payload(&inst).unwrap();
        assert_eq!(p["arch"], "HOPPER");
        assert_eq!(p["evidence_list"].as_array().unwrap().len(), 2);
        // Chutes-derived GPU nonce is NOT set here (verifier supplies it).
        assert!(p.get("nonce").is_none());
    }

    #[test]
    fn fail_closed_on_inconsistent_gpu_arch() {
        let mut g2 = gpu();
        g2.arch = "BLACKWELL".into();
        let inst = InstanceEvidence {
            quote: "BAACAIE=".into(),
            gpu_evidence: vec![gpu(), g2],
            instance_id: "i1".into(),
            certificate: TEST_CERT_B64.trim().into(),
        };
        assert!(matches!(
            nvidia_payload(&inst),
            Err(TransformError::InconsistentGpuArch { .. })
        ));
    }

    #[test]
    fn fail_closed_on_empty_gpu_arch() {
        let mut g = gpu();
        g.arch = "  ".into();
        let inst = InstanceEvidence {
            quote: "BAACAIE=".into(),
            gpu_evidence: vec![g],
            instance_id: "i1".into(),
            certificate: TEST_CERT_B64.trim().into(),
        };
        assert!(matches!(
            nvidia_payload(&inst),
            Err(TransformError::EmptyGpuArch)
        ));
    }

    #[test]
    fn base64_decoding_tolerates_surrounding_whitespace() {
        let inst = InstanceEvidence {
            quote: "  BAACAIE=\n".into(),
            gpu_evidence: vec![gpu()],
            instance_id: "i1".into(),
            certificate: TEST_CERT_B64.trim().into(),
        };
        assert_eq!(intel_quote_hex(&inst).unwrap(), "0400020081");
    }

    #[test]
    fn to_near_report_maps_all_shared_fields() {
        let inst = InstanceEvidence {
            quote: "BAACAIE=".into(),
            gpu_evidence: vec![gpu()],
            instance_id: "inst-7f3a".into(),
            certificate: TEST_CERT_B64.trim().into(),
        };
        let m = to_near_report(&inst, "deadbeef").unwrap();
        assert_eq!(m["intel_quote"], "0400020081");
        assert_eq!(m["request_nonce"], "deadbeef");
        assert_eq!(m["instance_id"], "inst-7f3a");
        assert_eq!(m["tls_cert_fingerprint"].as_str().unwrap().len(), 64);
        assert!(m["nvidia_payload"].is_string());
        // report_data-derived fields (signing_address/event_log) are NOT fabricated.
        assert!(m.get("signing_address").is_none());
        assert!(m.get("event_log").is_none());
    }

    #[test]
    fn fail_closed_on_bad_base64_quote() {
        let inst = InstanceEvidence {
            quote: "!!!not base64!!!".into(),
            gpu_evidence: vec![gpu()],
            instance_id: "i1".into(),
            certificate: TEST_CERT_B64.trim().into(),
        };
        assert!(matches!(
            intel_quote_hex(&inst),
            Err(TransformError::InvalidBase64 { field: "quote", .. })
        ));
    }

    #[test]
    fn fail_closed_on_no_gpu_evidence() {
        let inst = InstanceEvidence {
            quote: "BAACAIE=".into(),
            gpu_evidence: vec![],
            instance_id: "i1".into(),
            certificate: TEST_CERT_B64.trim().into(),
        };
        assert!(matches!(
            nvidia_payload(&inst),
            Err(TransformError::NoGpuEvidence)
        ));
    }

    #[test]
    fn fail_closed_on_empty_quote() {
        let inst = InstanceEvidence {
            quote: "".into(),
            gpu_evidence: vec![gpu()],
            instance_id: "i1".into(),
            certificate: TEST_CERT_B64.trim().into(),
        };
        assert!(matches!(
            intel_quote_hex(&inst),
            Err(TransformError::Empty("quote"))
        ));
    }
}
