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
    /// Reported TCB from the verified SNP report.
    pub tcb: RouterTcb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouterTcb {
    pub bootloader: u8,
    pub tee: u8,
    pub snp: u8,
    pub microcode: u8,
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
    /// Evidence or the router's model document could not be fetched or decoded.
    #[error("fetch error")]
    Fetch,
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
            Self::Fetch => "fetch_error",
        }
    }
}

/// A router host must be a bare lowercase ASCII `*.tinfoil.sh` hostname: no
/// scheme, port, path, userinfo, uppercase, trailing dot or IDN (`xn--` punycode
/// labels are rejected too). The ATC serves bundles for several router hosts and
/// the domain decides where requests are sent, so it is validated before it is
/// trusted. Pure: no I/O.
pub fn validate_router_domain(domain: &str) -> Result<(), TinfoilVerifyError> {
    const SUFFIX: &str = ".tinfoil.sh";
    let Some(prefix) = domain.strip_suffix(SUFFIX) else {
        return Err(TinfoilVerifyError::Malformed {
            stage: "router_domain",
        });
    };
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && !l.starts_with("xn--")
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if domain.len() > 253 || !prefix.split('.').all(label_ok) {
        return Err(TinfoilVerifyError::Malformed {
            stage: "router_domain",
        });
    }
    Ok(())
}

/// Verifies Tinfoil evidence. The session calls `validate_router_domain` itself
/// on the verified bundle's domain, as a deliberate seam guard (the domain picks
/// the request host, so the session never relies on the verifier alone), but
/// implementations MUST still enforce it, and MUST also check that the attested
/// certificate's SAN names that domain, inside `verify_router`.
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
