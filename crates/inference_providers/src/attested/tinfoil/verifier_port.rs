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
    /// Sigstore (DSSE/Rekor) bundle over the ATC document. Intentionally an
    /// opaque pass-through here: the runtime verifier does not read it (it
    /// anchors on the SNP report chain plus the compiled pins). It is verified
    /// only by the offline `tinfoil_sync` tooling, whose Sigstore dependencies
    /// must stay out of this crate's (and `api`'s) dependency tree, so no typed
    /// bundle lives in this port.
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

/// A proxy model entry whose registers, repo and tag all matched one compiled
/// pin for `slug`. `entry.repo` and `entry.tag` are therefore the pinned
/// provenance.
#[derive(Debug, Clone)]
pub struct PinnedModel {
    pub slug: String,
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
    /// `stage` names the decode step that failed (a fixed, non-sensitive
    /// tag); it is for diagnosis only and is not part of [`Self::reason`].
    #[error("malformed evidence ({stage})")]
    Malformed { stage: &'static str },
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
            Self::Malformed { .. } => "malformed_evidence",
        }
    }
}

pub trait TinfoilVerifier: Send + Sync {
    fn verify_router(&self, bundle: &AtcBundle) -> Result<VerifiedRouter, TinfoilVerifyError>;
    /// Verifies that `entry` matches a compiled pin for `slug`: the measurement
    /// registers, `repo` and `tag` must all equal one pinned row. Enclave
    /// metadata (`enclaves`) is passed through unverified. `entry` is
    /// caller-supplied, so callers must obtain it from a source whose
    /// integrity is established separately (the router's attested TLS
    /// channel); a pin match alone does not authenticate where it came from.
    fn check_model(
        &self,
        slug: &str,
        entry: &ProxyModelEntry,
    ) -> Result<PinnedModel, TinfoilVerifyError>;
}
