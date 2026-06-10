//! Serde models for Chutes' TEE evidence response.
//!
//! Shape verified against the live `GET /chutes/{chute_id}/evidence?nonce=<64-hex>`
//! response (2026-06): the endpoint returns evidence for *every* live instance of
//! a chute. Each instance carries an Intel TDX quote, NVIDIA GPU confidential-
//! compute evidence (one entry per physical GPU — 8 on an H200 node), the
//! instance id, and the instance's self-signed attestation certificate.
//!
//! ```jsonc
//! {
//!   "evidence": [
//!     {
//!       "quote": "<base64 Intel TDX v4 quote>",
//!       "gpu_evidence": [ { "certificate": "<base64>", "evidence": "<base64 SPDM>", "arch": "HOPPER" }, ... ],
//!       "instance_id": "<uuid>",
//!       "certificate": "<base64 DER X.509 self-signed instance cert>"
//!     }
//!   ],
//!   "failed_instance_ids": []
//! }
//! ```
//!
//! The TDX quote's `report_data` binds freshness + identity (per Chutes' docs and
//! our own decoding): `report_data[0:32] = SHA256(nonce ‖ instance_e2e_pubkey)`
//! (the ML-KEM-768 key from `GET /e2e/instances/{chute_id}`) and
//! `report_data[32:64] = SHA256(SPKI(certificate))`. Verifying those bindings is
//! the job of the Chutes-specific verifier (a later PR); this module only models
//! the wire shape and extracts the fields.

use serde::{Deserialize, Deserializer};

/// Deserialize a value that may be absent **or explicitly `null`** into its
/// `Default`. `#[serde(default)]` alone only covers an absent key; Python-backed
/// APIs (like Chutes') commonly emit `null` for empty lists, which would
/// otherwise fail deserialization of the whole response.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Top-level response of `GET /chutes/{chute_id}/evidence?nonce=...`.
#[derive(Debug, Clone, Deserialize)]
pub struct EvidenceResponse {
    /// One entry per live instance of the chute. Null-tolerant for symmetry with
    /// the sibling list fields: a chute with zero live instances can come back as
    /// `"evidence": null`, which must reach the clean "instance not found" path
    /// rather than fail the whole parse with an opaque serde error.
    #[serde(default, deserialize_with = "null_default")]
    pub evidence: Vec<InstanceEvidence>,
    /// Instances whose evidence collection failed (best-effort; informational —
    /// tolerant of an absent key or an explicit `null`).
    #[serde(default, deserialize_with = "null_default")]
    pub failed_instance_ids: Vec<String>,
}

/// TEE evidence for a single Chutes instance.
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceEvidence {
    /// Base64-encoded raw Intel TDX quote.
    pub quote: String,
    /// Per-GPU NVIDIA confidential-compute evidence (tolerant of absent/`null`).
    #[serde(default, deserialize_with = "null_default")]
    pub gpu_evidence: Vec<GpuEvidence>,
    /// Instance identifier (UUID).
    pub instance_id: String,
    /// Base64-encoded DER of the instance's self-signed X.509 attestation cert.
    /// Its SPKI fingerprint is bound into the quote's `report_data[32:64]`.
    pub certificate: String,
}

/// NVIDIA confidential-compute evidence for one physical GPU.
#[derive(Debug, Clone, Deserialize)]
pub struct GpuEvidence {
    /// Base64 NVIDIA device certificate chain.
    pub certificate: String,
    /// Base64 SPDM measurement/attestation evidence (submitted to NVIDIA NRAS).
    pub evidence: String,
    /// GPU architecture, e.g. `"HOPPER"`.
    pub arch: String,
}

impl EvidenceResponse {
    /// Find the evidence for a specific instance id.
    pub fn instance(&self, instance_id: &str) -> Option<&InstanceEvidence> {
        self.evidence.iter().find(|e| e.instance_id == instance_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "evidence": [
            {
                "quote": "BAACAIE=",
                "gpu_evidence": [
                    { "certificate": "Y2VydA==", "evidence": "ZXZpZA==", "arch": "HOPPER" },
                    { "certificate": "Y2VydDI=", "evidence": "ZXZpZDI=", "arch": "HOPPER" }
                ],
                "instance_id": "d3a2c829-ab6f-4469-ae2f-5c56e0adc225",
                "certificate": "TUlJRg=="
            }
        ],
        "failed_instance_ids": []
    }"#;

    #[test]
    fn parses_real_shape() {
        let r: EvidenceResponse = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(r.evidence.len(), 1);
        assert!(r.failed_instance_ids.is_empty());
        let inst = &r.evidence[0];
        assert_eq!(inst.instance_id, "d3a2c829-ab6f-4469-ae2f-5c56e0adc225");
        assert_eq!(inst.gpu_evidence.len(), 2);
        assert_eq!(inst.gpu_evidence[0].arch, "HOPPER");
    }

    #[test]
    fn instance_lookup() {
        let r: EvidenceResponse = serde_json::from_str(SAMPLE).unwrap();
        assert!(r.instance("d3a2c829-ab6f-4469-ae2f-5c56e0adc225").is_some());
        assert!(r.instance("nope").is_none());
    }

    #[test]
    fn missing_failed_ids_defaults_empty() {
        let r: EvidenceResponse =
            serde_json::from_str(r#"{"evidence":[]}"#).expect("failed_instance_ids is optional");
        assert!(r.failed_instance_ids.is_empty());
    }

    #[test]
    fn explicit_null_evidence_tolerated() {
        // A chute with zero live instances may come back as `"evidence": null`.
        // It must parse to an empty list and reach the clean "instance not found"
        // path, not fail the whole response with an opaque serde error.
        let r: EvidenceResponse =
            serde_json::from_str(r#"{"evidence":null,"failed_instance_ids":[]}"#)
                .expect("explicit null evidence should deserialize to empty");
        assert!(r.evidence.is_empty());
        assert!(r.instance("anything").is_none());
    }

    #[test]
    fn explicit_null_lists_tolerated() {
        // Python-backed APIs often emit null for empty lists; must not fail the
        // whole parse (gpu_evidence + failed_instance_ids are null-tolerant).
        let r: EvidenceResponse = serde_json::from_str(
            r#"{"evidence":[{"quote":"q","gpu_evidence":null,"instance_id":"i","certificate":"c"}],"failed_instance_ids":null}"#,
        )
        .expect("explicit null lists should deserialize to empty");
        assert!(r.failed_instance_ids.is_empty());
        assert!(r.evidence[0].gpu_evidence.is_empty());
    }
}
