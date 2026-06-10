//! Chutes attested-provider verifier.
//!
//! Ties the Chutes-specific verification primitives (defined in
//! `inference_providers::attested::chutes`) to the shared, audited DCAP quote
//! verification and NVIDIA NRAS GPU verification (in [`super::verification`]):
//!
//! 1. **DCAP quote** — `AttestationVerifier::verify_tdx_quote` (Intel signature
//!    chain, TCB floor, debug bit). Shared verbatim with NEAR.
//! 2. **`report_data` bindings** — Chutes-specific: `report_data[0:32] =
//!    SHA256(nonce ‖ e2e_pubkey)` (freshness + E2EE-key binding) and
//!    `report_data[32:64] = SHA256(SPKI(cert))`.
//! 3. **Measurement** — register-pin MRTD + RTMR0-2 against a vetted snapshot of
//!    Chutes' published golden values (Chutes ships no RTMR3 event log, so this
//!    replaces NEAR's event-log replay + image-hash path).
//! 4. **GPU** — NVIDIA NRAS, with the *Chutes-derived* nonce
//!    (`SHA256(nonce ‖ e2e_pubkey)`, the same value sealed in `report_data[0:32]`
//!    and bound into the GPU SPDM evidence), not the raw caller nonce.
//!
//! On success the caller can open an ML-KEM-768 E2EE channel to the verified
//! `e2e_pubkey` (see `inference_providers::attested::chutes::e2ee`) knowing it is
//! bound to attested, vetted software. Everything is fail-closed: an empty
//! measurement allow-list, any binding mismatch, an unvetted measurement, or
//! absent GPU evidence is an error, never a soft pass.

use std::collections::HashSet;

use inference_providers::attested::chutes::attestation as transform;
use inference_providers::attested::chutes::evidence::InstanceEvidence;
use inference_providers::attested::chutes::measurements::{
    BootMeasurement, ChutesMeasurementPolicy, MeasurementError,
};
use inference_providers::attested::chutes::report_data::{
    freshness_digest, ChutesReportDataVerifier, ReportDataError,
};

use inference_providers::attested::chutes::verifier_port::{
    ChutesInstanceVerifier, VerifiedInstanceInfo,
};

use super::measurement::MeasurementPolicy;
use super::verification::{AttestationVerificationError, AttestationVerifier};

/// Failure of the end-to-end Chutes instance verification. Every variant is
/// fatal — the trust chain holds only if all four stages pass.
#[derive(Debug, thiserror::Error)]
pub enum ChutesVerifyError {
    #[error("evidence transform: {0}")]
    Transform(#[from] transform::TransformError),
    #[error("TDX quote / GPU verification: {0}")]
    Verifier(#[from] AttestationVerificationError),
    #[error("report_data binding: {0}")]
    ReportData(#[from] ReportDataError),
    #[error("measurement register-pin: {0}")]
    Measurement(#[from] MeasurementError),
    #[error("verified quote is not a TDX TD1.0 report")]
    NotTd10,
    #[error("GPU evidence payload is not a JSON object (cannot bind the GPU nonce)")]
    MalformedGpuPayload,
    #[error("GPU evidence required but the verifier returned no verdict")]
    MissingGpuVerdict,
}

/// A Chutes instance whose full attestation chain verified.
#[derive(Debug, Clone)]
pub struct ChutesVerifiedInstance {
    /// The instance the evidence belongs to.
    pub instance_id: String,
    /// The attested ML-KEM-768 `e2e_pubkey` (base64) — safe to encapsulate to.
    pub e2e_pubkey: String,
    /// Matched golden config, e.g. `"8xh200 v1.3.0"`.
    pub measurement_config: String,
    /// TDX TCB status (`"UpToDate"` when the policy's floor is met).
    pub tcb_status: String,
    /// NVIDIA NRAS verdict (`"PASS"`).
    pub gpu_verdict: String,
}

/// Verifies Chutes instances end to end. Holds an inner [`AttestationVerifier`]
/// used **only** for the shared DCAP-quote and NRAS-GPU steps, plus the
/// register-pin [`ChutesMeasurementPolicy`].
pub struct ChutesBackendVerifier {
    inner: AttestationVerifier,
    measurement_policy: ChutesMeasurementPolicy,
}

impl ChutesBackendVerifier {
    /// Build a verifier from a vetted golden-measurement snapshot.
    pub fn new(measurement_policy: ChutesMeasurementPolicy, pccs_url: Option<String>) -> Self {
        // `attested3p` gives the flags we need for the shared steps:
        // require_tcb_up_to_date = true and require_gpu_evidence = true. Its
        // image-hash allowlist is intentionally empty and unused — Chutes
        // measurement is register-pinned via `measurement_policy`, and we never
        // call the inner verifier's `verify_attestation_report` (only
        // `verify_tdx_quote` + `verify_gpu_evidence`), so the allowlist is never
        // consulted. Were it ever consulted, an empty attested3p allowlist
        // fails closed.
        let inner = AttestationVerifier::with_policy(
            MeasurementPolicy::attested3p(HashSet::new()),
            pccs_url,
        );
        Self {
            inner,
            measurement_policy,
        }
    }

    /// Verify a single instance's evidence end to end.
    ///
    /// - `evidence` — the `/evidence` entry for this instance (quote, GPU
    ///   evidence, certificate).
    /// - `boot_nonce` — the nonce used in the `/evidence` query (the freshness
    ///   anchor sealed into `report_data[0:32]`).
    /// - `e2e_pubkey` — the base64 ML-KEM-768 key from `/e2e/instances` for this
    ///   instance.
    pub async fn verify_instance(
        &self,
        evidence: &InstanceEvidence,
        boot_nonce: &str,
        e2e_pubkey: &str,
    ) -> Result<ChutesVerifiedInstance, ChutesVerifyError> {
        // Fail-closed up front: refuse if no golden measurements are configured.
        self.measurement_policy.assert_enforceable()?;

        // 1. DCAP-verify the TDX quote (signature chain, TCB floor, debug bit).
        let quote_hex = transform::intel_quote_hex(evidence)?;
        let verified = self.inner.verify_tdx_quote(&quote_hex).await?;
        let tcb_status = verified.status.clone();
        let td = verified
            .report
            .as_td10()
            .ok_or(ChutesVerifyError::NotTd10)?;

        // 2. report_data bindings: freshness + e2e-key [0:32], cert SPKI [32:64].
        let cert_der = transform::certificate_der(evidence)?;
        ChutesReportDataVerifier.verify(&td.report_data, boot_nonce, e2e_pubkey, &cert_der)?;

        // 3. Register-pin the boot chain (MRTD + RTMR0-2) to a vetted config.
        let matched = self
            .measurement_policy
            .verify(&td.mr_td, &td.rt_mr0, &td.rt_mr1, &td.rt_mr2)?;
        let measurement_config = format!("{} v{}", matched.name, matched.version);

        // 4. GPU: the SPDM evidence is bound to the Chutes-derived nonce — the
        //    same SHA256(boot_nonce ‖ e2e_pubkey) that lands in report_data[0:32]
        //    — not the raw caller nonce. Inject it and verify via NRAS.
        let gpu_nonce = hex::encode(freshness_digest(boot_nonce, e2e_pubkey));
        let mut nvidia_payload = transform::nvidia_payload(evidence)?;
        // Fatal if the payload isn't an object: proceeding without injecting the
        // nonce would submit GPU evidence unbound to our freshness anchor.
        nvidia_payload
            .as_object_mut()
            .ok_or(ChutesVerifyError::MalformedGpuPayload)?
            .insert(
                "nonce".to_string(),
                serde_json::Value::String(gpu_nonce.clone()),
            );
        let mut report = serde_json::Map::new();
        report.insert(
            "nvidia_payload".to_string(),
            serde_json::Value::String(nvidia_payload.to_string()),
        );
        let gpu_verdict = self
            .inner
            .verify_gpu_evidence(&report, &gpu_nonce)
            .await?
            .ok_or(ChutesVerifyError::MissingGpuVerdict)?;

        Ok(ChutesVerifiedInstance {
            instance_id: evidence.instance_id.clone(),
            e2e_pubkey: e2e_pubkey.to_string(),
            measurement_config,
            tcb_status,
            gpu_verdict,
        })
    }
}

/// Dependency-inversion seam: let the `inference_providers` Chutes `Provider`
/// (which can't depend on `services`) drive this verifier through a narrow port.
/// Maps the rich [`ChutesVerifiedInstance`] to the port's [`VerifiedInstanceInfo`]
/// and flattens the typed error to a safe string (no secrets/plaintext).
#[async_trait::async_trait]
impl ChutesInstanceVerifier for ChutesBackendVerifier {
    async fn attest_instance(
        &self,
        evidence: &InstanceEvidence,
        boot_nonce: &str,
        e2e_pubkey: &str,
    ) -> Result<VerifiedInstanceInfo, String> {
        // Inherent `verify_instance` (not the trait method) does the work.
        self.verify_instance(evidence, boot_nonce, e2e_pubkey)
            .await
            .map(|v| VerifiedInstanceInfo {
                instance_id: v.instance_id,
                e2e_pubkey: v.e2e_pubkey,
                measurement_config: v.measurement_config,
                tcb_status: v.tcb_status,
                gpu_verdict: v.gpu_verdict,
            })
            .map_err(|e| e.to_string())
    }
}

/// A vetted snapshot of Chutes' published golden measurements (from the public
/// `GET https://api.chutes.ai/servers/tee/measurements`), confirmed 2026-06-10 to
/// match the live GLM-5.1-TEE fleet byte-for-byte on MRTD + RTMR0-2. RTMR3 is a
/// runtime register and intentionally not pinned. This is the software-identity
/// anchor for the staging rollout; expand (or make config-driven) as more configs
/// are independently vetted. Fail-closed: anything not listed here is rejected.
pub fn vetted_golden_measurements() -> ChutesMeasurementPolicy {
    ChutesMeasurementPolicy::new(vec![BootMeasurement::new(
        "8xh200",
        "1.3.0",
        "ddc6efcdd2309e10837f8a7f64b71272b7ef003b129460410fe715bdfffec38c7c0c1686dddb2a23d4fd623d145e8455",
        "2864b11878e8129095d62a5dd7c3e3aae178d3a077606a825617324768f189ad05aed08376947df92d6c75865d915cbf",
        "f858ed2aecba4ecd29084352c6b5c6e403c0bec89b8c852f90fa5a8cee796ffa095518c5cd8b92c25c1856e932a95877",
        "7719f4fde518994a5dd6767a8b8b87a38168cc0f3480e7498d4ace99e49319be6a7fed26c21ad43310d2d488fc68ab1c",
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_evidence(quote: &str) -> InstanceEvidence {
        InstanceEvidence {
            quote: quote.to_string(),
            gpu_evidence: vec![],
            instance_id: "inst-1".to_string(),
            certificate: "Y2VydA==".to_string(),
        }
    }

    fn glm_policy() -> ChutesMeasurementPolicy {
        // Valid 48-byte (96 hex char) registers so assert_enforceable passes and
        // the flow can reach the transform stage.
        let reg = "dd".repeat(48);
        ChutesMeasurementPolicy::new(vec![BootMeasurement::new(
            "8xh200", "1.3.0", &reg, &reg, &reg, &reg,
        )])
    }

    // The expensive stages (DCAP collateral fetch, NRAS) need the network and a
    // real signed quote; they're proven by the live round-trip before flag-on.
    // These tests lock the *fail-closed ordering* that must hold without network.

    #[tokio::test]
    async fn empty_measurement_policy_fails_closed_before_network() {
        // No golden values configured -> reject immediately, never touching DCAP.
        let v = ChutesBackendVerifier::new(ChutesMeasurementPolicy::new(vec![]), None);
        let err = v
            .verify_instance(&dummy_evidence("BAACAIE="), &"a".repeat(64), "pk")
            .await
            .unwrap_err();
        assert!(matches!(err, ChutesVerifyError::Measurement(_)));
    }

    #[tokio::test]
    async fn port_attest_instance_maps_error_to_string() {
        // Through the dependency-inversion port, a fail-closed rejection surfaces
        // as an Err(String) (no panic, no secrets) — the provider treats it fatal.
        let v = ChutesBackendVerifier::new(ChutesMeasurementPolicy::new(vec![]), None);
        let err = ChutesInstanceVerifier::attest_instance(
            &v,
            &dummy_evidence("BAACAIE="),
            &"a".repeat(64),
            "pk",
        )
        .await
        .unwrap_err();
        assert!(err.contains("measurement") || err.contains("attest"));
    }

    #[tokio::test]
    async fn malformed_quote_fails_in_transform_before_network() {
        // Past the policy guard, a non-base64 quote fails in the transform
        // (still before any DCAP network call).
        let v = ChutesBackendVerifier::new(glm_policy(), None);
        let err = v
            .verify_instance(&dummy_evidence("!!! not base64 !!!"), &"a".repeat(64), "pk")
            .await
            .unwrap_err();
        assert!(matches!(err, ChutesVerifyError::Transform(_)));
    }
}
