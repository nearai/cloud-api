//! Dependency-inversion seam for verifying Tinfoil evidence. The concrete
//! policy verifier lives in `services`; the provider depends on this port.

use std::collections::BTreeMap;

/// The ATC (`atc.tinfoil.sh/attestation`) bundle for the router.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AtcBundle {
    pub domain: String,
    #[serde(rename = "enclaveAttestationReport")]
    pub report: AttestationDoc,
    pub vcek: String,
    #[serde(rename = "enclaveCert")]
    pub enclave_cert: String,
    pub digest: String,
    #[serde(rename = "sigstoreBundle")]
    pub sigstore_bundle: serde_json::Value,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AttestationDoc {
    pub format: String,
    pub body: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ProxyDoc {
    pub models: BTreeMap<String, ProxyModelEntry>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ProxyModelEntry {
    pub repo: String,
    pub tag: String,
    pub measurement: ProxyMeasurement,
    pub enclaves: BTreeMap<String, ProxyEnclave>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ProxyMeasurement {
    #[serde(rename = "type")]
    pub kind: String,
    pub registers: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ProxyEnclave {
    pub tls_key_fp: String,
    pub hpke_key: String,
    pub predicate: String,
}

/// A router whose attestation verified against pins and is bound to its TLS key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRouter {
    pub spki_sha256: [u8; 32],
    pub measurement_hex: String,
    pub tag: String,
}

#[derive(Debug, Clone)]
pub struct PinnedModel {
    pub slug: String,
    pub repo: String,
    pub tag: String,
    pub entry: ProxyModelEntry,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum TinfoilVerifyError {
    #[error("unsupported router platform")]
    UnsupportedRouterPlatform,
    #[error("unknown router measurement")]
    UnknownRouterMeasurement,
    #[error("unknown model measurement")]
    UnknownModelMeasurement,
    #[error("bad signature")]
    BadSignature,
    #[error("debug policy")]
    DebugPolicy,
    #[error("tcb too low")]
    TcbTooLow,
    #[error("report data mismatch")]
    ReportDataMismatch,
    #[error("malformed evidence")]
    Malformed,
}

impl TinfoilVerifyError {
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::UnsupportedRouterPlatform => "unsupported_router_platform",
            Self::UnknownRouterMeasurement => "unknown_router_measurement",
            Self::UnknownModelMeasurement => "unknown_model_measurement",
            Self::BadSignature => "bad_signature",
            Self::DebugPolicy => "debug_policy",
            Self::TcbTooLow => "tcb_too_low",
            Self::ReportDataMismatch => "report_data_mismatch",
            Self::Malformed => "malformed_evidence",
        }
    }
}

pub trait TinfoilVerifier: Send + Sync {
    fn verify_router(&self, bundle: &AtcBundle) -> Result<VerifiedRouter, TinfoilVerifyError>;
    fn check_model(
        &self,
        slug: &str,
        entry: &ProxyModelEntry,
    ) -> Result<PinnedModel, TinfoilVerifyError>;
}
