//! Offline Sigstore verification of a Tinfoil release attestation.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigstoreResult {
    pub repo: String,
    pub tag: String,
    pub predicate_type: String,
    pub subject_sha256: String,
    pub snp_measurement: String,
    pub rtmr1: Option<String>,
    pub rtmr2: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SigstoreError {
    #[error("sigstore: {0}")]
    Invalid(String),
}

const OIDC_ISSUER: &str = "https://token.actions.githubusercontent.com";
const WORKFLOW: &str = ".github/workflows/tinfoil-release-publish.yml";
const SUBJECT_NAME: &str = "tinfoil-deployment.json";

fn bad(msg: impl std::fmt::Display) -> SigstoreError {
    SigstoreError::Invalid(msg.to_string())
}

/// Verify a Sigstore bundle for a release of `repo` (`owner/name`): Fulcio
/// certificate chain, Rekor inclusion and the DSSE signature, all offline
/// against the embedded public-good trusted root. The signer must be GitHub
/// Actions running `tinfoil-release-publish.yml` in `repo` at a tag. Returns
/// the tag and the measurements of the signed in-toto predicate.
pub fn verify_bundle(
    bundle: &serde_json::Value,
    repo: &str,
) -> Result<SigstoreResult, SigstoreError> {
    verify_bundle_for(bundle, repo, WORKFLOW, OIDC_ISSUER)
}

fn verify_bundle_for(
    bundle: &serde_json::Value,
    repo: &str,
    workflow: &str,
    issuer: &str,
) -> Result<SigstoreResult, SigstoreError> {
    use base64::Engine;
    use sigstore_verify::trust_root::{SigstoreInstance, TrustedRoot};
    use sigstore_verify::types::{Bundle, Sha256Hash};
    use sigstore_verify::{VerificationPolicy, Verifier};

    let parsed: Bundle = serde_json::from_value(bundle.clone()).map_err(|_| bad("bundle shape"))?;
    let payload_b64 = bundle["dsseEnvelope"]["payload"]
        .as_str()
        .ok_or_else(|| bad("not a DSSE bundle"))?;
    let payload = base64::engine::general_purpose::STANDARD
        .decode(payload_b64)
        .map_err(|_| bad("payload encoding"))?;
    let stmt: serde_json::Value =
        serde_json::from_slice(&payload).map_err(|_| bad("payload is not JSON"))?;
    let subjects = stmt["subject"]
        .as_array()
        .ok_or_else(|| bad("no subject"))?;
    let [subject] = subjects.as_slice() else {
        return Err(bad("expected exactly one subject"));
    };
    if subject["name"] != SUBJECT_NAME {
        return Err(bad("unexpected subject name"));
    }
    let subject_sha256 = subject["digest"]["sha256"]
        .as_str()
        .ok_or_else(|| bad("no subject digest"))?
        .to_string();
    let digest = Sha256Hash::from_hex(&subject_sha256).map_err(|_| bad("subject digest"))?;

    let root = TrustedRoot::from_embedded(SigstoreInstance::PublicGood).map_err(bad)?;
    let verifier = Verifier::new(&root).map_err(bad)?;
    // The tag is part of the signer identity, so the identity is checked
    // below instead of through an exact-match policy.
    let policy = VerificationPolicy::any_identity().require_issuer(issuer);
    let result = verifier.verify(digest, &parsed, &policy).map_err(bad)?;

    let san = result.identity().ok_or_else(|| bad("no signer identity"))?;
    let prefix = format!("https://github.com/{repo}/{workflow}@refs/tags/");
    let tag = san
        .as_str()
        .strip_prefix(&prefix)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| bad("signer identity does not match repo"))?
        .to_string();

    let predicate = &stmt["predicate"];
    let hex_field = |v: &serde_json::Value| v.as_str().map(str::to_string);
    let snp_measurement =
        hex_field(&predicate["snp_measurement"]).ok_or_else(|| bad("no snp_measurement"))?;
    Ok(SigstoreResult {
        repo: repo.to_string(),
        tag,
        predicate_type: stmt["predicateType"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        subject_sha256,
        snp_measurement,
        rtmr1: hex_field(&predicate["tdx_measurement"]["rtmr1"]),
        rtmr2: hex_field(&predicate["tdx_measurement"]["rtmr2"]),
    })
}

impl SigstoreResult {
    /// The registers `/.well-known/tinfoil-proxy` publishes for a
    /// `snp-tdx-multiplatform` release: SNP measurement, RTMR1, RTMR2.
    pub fn registers(&self) -> Option<Vec<String>> {
        Some(vec![
            self.snp_measurement.clone(),
            self.rtmr1.clone()?,
            self.rtmr2.clone()?,
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atc() -> serde_json::Value {
        serde_json::from_str(include_str!("../testdata/atc_attestation.out")).unwrap()
    }

    #[test]
    fn sigstore_bundle_from_probe_verifies_and_identity_matches() {
        let b = atc();
        let r = verify_bundle(&b["sigstoreBundle"], "tinfoilsh/confidential-model-router").unwrap();
        assert_eq!(r.tag, "v0.0.155");
        assert_eq!(r.snp_measurement, "b3be62c7199d8e4d24f130e5651bdc8a62a2532f72c7e87c986bec54bf5f90bab703ad4dbfc5e45bfd385f8972dfc66c");
        assert_eq!(
            r.predicate_type,
            "https://tinfoil.sh/predicate/snp-tdx-multiplatform/v1"
        );
    }

    #[test]
    fn model_bundle_verifies_with_registers() {
        let b: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/model_sigstore_bundle.json")).unwrap();
        let r = verify_bundle(&b, "tinfoilsh/confidential-deepseek-v4-1-flash").unwrap();
        assert_eq!(r.tag, "v0.0.3");
        assert_eq!(
            r.subject_sha256,
            "8448fed68f4ed10c829a6433bdf570fa12a7b0e6d589bf1a89a8e9a5ca37a3ac"
        );
        assert_eq!(r.rtmr1.as_deref(), Some("3246c8c822eade4b4ac78e5817776032a371d50475950395ff357076e9f991f62d7bf5b0ce973799f36b0ab97387e51d"));
    }

    #[test]
    fn sigstore_rejects_wrong_repo_identity() {
        let b = atc();
        assert!(verify_bundle(&b["sigstoreBundle"], "tinfoilsh/other").is_err());
    }

    #[test]
    fn sigstore_rejects_tampered_payload() {
        use base64::Engine;
        let mut b = atc();
        let e = base64::engine::general_purpose::STANDARD;
        let mut payload = e
            .decode(
                b["sigstoreBundle"]["dsseEnvelope"]["payload"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
        // Flip a byte inside the snp_measurement value: the statement stays
        // well formed, so only the DSSE signature check can reject it.
        let marker = br#""snp_measurement":""#;
        let at = payload
            .windows(marker.len())
            .position(|w| w == marker)
            .unwrap()
            + marker.len();
        payload[at] = if payload[at] == b'0' { b'1' } else { b'0' };
        b["sigstoreBundle"]["dsseEnvelope"]["payload"] = e.encode(payload).into();
        let err = verify_bundle(&b["sigstoreBundle"], "tinfoilsh/confidential-model-router")
            .unwrap_err()
            .to_string();
        assert!(err.contains("DSSE signature"), "unexpected error: {err}");
    }

    #[test]
    fn sigstore_rejects_wrong_workflow_in_expected_identity() {
        let b = atc();
        let err = verify_bundle_for(
            &b["sigstoreBundle"],
            "tinfoilsh/confidential-model-router",
            ".github/workflows/other.yml",
            OIDC_ISSUER,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("does not match repo"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn sigstore_rejects_wrong_oidc_issuer() {
        let b = atc();
        assert!(verify_bundle_for(
            &b["sigstoreBundle"],
            "tinfoilsh/confidential-model-router",
            WORKFLOW,
            "https://issuer.example",
        )
        .is_err());
    }
}
