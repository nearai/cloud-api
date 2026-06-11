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
//! 3. **Measurement** — register-pin MRTD + RTMR0-2 (boot chain) **and the
//!    runtime RTMR3** (running app/IMA layer) against a vetted snapshot of
//!    Chutes' published golden values (`runtime_rtmrs.RTMR3`). This replaces
//!    NEAR's event-log replay + image-hash path and authenticates the full
//!    software identity, not just the boot chain.
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
    ChutesMeasurementPolicy, ExpectedMeasurement, MeasurementError,
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

        // 3. Register-pin the full chain — MRTD + RTMR0-2 (boot: firmware/kernel/
        //    cmdline) AND the runtime RTMR3 (running app/IMA layer) — to a vetted
        //    config (Chutes publishes the runtime RTMR3 in `runtime_rtmrs`).
        let matched = self
            .measurement_policy
            .verify(&td.mr_td, &td.rt_mr0, &td.rt_mr1, &td.rt_mr2, &td.rt_mr3)?;
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
/// `GET https://api.chutes.ai/servers/tee/measurements`) for the **v1.3.0**
/// software release across all of Chutes' GPU hardware platforms.
///
/// Within v1.3.0 the five rows below are byte-identical on MRTD (firmware),
/// RTMR1 (kernel), RTMR2 (cmdline/initrd) **and the runtime RTMR3**
/// (`runtime_rtmrs.RTMR3`, the running app/IMA layer) — i.e. the full *software*
/// identity is one and the same. They differ **only in RTMR0**, the TD-HOB/ACPI/
/// VM-config layer that varies with the GPU SKU and VM sizing (8×H200,
/// 8×RTX PRO 6000, 8×B200, 8×B200-eth, 8×B300). Pinning each published RTMR0
/// lets a request land on any of Chutes' vetted hardware platforms while still
/// authenticating the full software stack against a single, fixed identity.
///
/// Live cross-checks against genuine, DCAP-signature-verified, report-data-bound
/// quotes (the measurement check runs only *after* the Intel signature chain and
/// the nonce/e2e-key/SPKI bindings pass, so observed registers are trustworthy):
///   - `8xh200 v1.3.0`         — GLM-5.1-TEE, 6/6 quotes (2026-06-10); and
///                               kimi-k2.5 / deepseek-v3.2 / minimax-m2.5 served
///                               live over E2EE (2026-06-11).
///   - `8xRTX_PRO_6000 v1.3.0` — Qwen3-32B-TEE, observed register set matched the
///                               published row byte-for-byte (2026-06-11).
/// The remaining Blackwell rows (b200 / b200-eth / b300) are taken from the same
/// published v1.3.0 release and carry the identical software identity; they are
/// accepted so a model scheduled onto Blackwell hardware serves without a
/// per-SKU re-vet. The cryptographic root of trust is the Intel DCAP signature,
/// not the transparency endpoint — these rows merely enumerate which genuine
/// software identities we accept.
///
/// NOT included (fail-closed): the v1.0.0–v1.2.0 rows publish RTMR3 = all-zeros
/// (a boot template, never matchable against a live extended RTMR3), and v1.3.1
/// is a distinct firmware/kernel (different MRTD/RTMR1) with no live instance yet
/// — add it once Chutes upgrades and a live quote cross-checks. Anything not
/// listed here is rejected.
pub fn vetted_golden_measurements() -> ChutesMeasurementPolicy {
    // v1.3.0 software identity — shared across every hardware row below.
    const MRTD: &str = "ddc6efcdd2309e10837f8a7f64b71272b7ef003b129460410fe715bdfffec38c7c0c1686dddb2a23d4fd623d145e8455";
    const RTMR1: &str = "f858ed2aecba4ecd29084352c6b5c6e403c0bec89b8c852f90fa5a8cee796ffa095518c5cd8b92c25c1856e932a95877";
    const RTMR2: &str = "7719f4fde518994a5dd6767a8b8b87a38168cc0f3480e7498d4ace99e49319be6a7fed26c21ad43310d2d488fc68ab1c";
    // runtime RTMR3 (runtime_rtmrs.RTMR3) — the running app/IMA layer.
    const RTMR3: &str = "bfac8bbe97148d00c0bc5dea273ccd926e2415511f08f5dedaa96d3c19e824d2bf01fae86e8987ff509fd3ad31374a60";

    // (config name, RTMR0) — the per-hardware boot/VM-config register.
    let hardware_rows = [
        ("8xh200", "2864b11878e8129095d62a5dd7c3e3aae178d3a077606a825617324768f189ad05aed08376947df92d6c75865d915cbf"),
        ("8xRTX_PRO_6000", "5064826bfd530ca9f823ceecb74899d7dbd014b60897a77317a14200c8706f2368ecbbc0a04cec8ceef90474b8c955e1"),
        ("8xb200", "734628b9a715ec492c2b14b409907f32d91847f439ba8bac2fa985b41c01245536348fefb2e021ed574c290c8c50347a"),
        ("8xb200-eth", "724c1d0d20c11a479d2874fa543b0f1b920be32f2a5b9707fa5bcf6176fff31aeac9436e541e1125f78a0b61f7c2e165"),
        ("8xb300", "31f6446add906b7d56132c600549270a8ea780193e0c89586f784b20b25136de441ca715d5ecf86ae72f0b40f7a47f39"),
    ];

    ChutesMeasurementPolicy::new(
        hardware_rows
            .iter()
            .map(|&(name, rtmr0)| {
                ExpectedMeasurement::new(name, "1.3.0", MRTD, rtmr0, RTMR1, RTMR2, RTMR3)
            })
            .collect(),
    )
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
        ChutesMeasurementPolicy::new(vec![ExpectedMeasurement::new(
            "8xh200", "1.3.0", &reg, &reg, &reg, &reg, &reg,
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

    mod golden {
        use super::*;
        use inference_providers::attested::chutes::measurements::REGISTER_LEN;

        fn reg(h: &str) -> [u8; REGISTER_LEN] {
            let v = hex::decode(h).expect("valid hex register");
            assert_eq!(v.len(), REGISTER_LEN, "register must be 48 bytes");
            let mut a = [0u8; REGISTER_LEN];
            a.copy_from_slice(&v);
            a
        }

        // The v1.3.0 software identity, shared across every hardware row.
        const MRTD: &str = "ddc6efcdd2309e10837f8a7f64b71272b7ef003b129460410fe715bdfffec38c7c0c1686dddb2a23d4fd623d145e8455";
        const RTMR1: &str = "f858ed2aecba4ecd29084352c6b5c6e403c0bec89b8c852f90fa5a8cee796ffa095518c5cd8b92c25c1856e932a95877";
        const RTMR2: &str = "7719f4fde518994a5dd6767a8b8b87a38168cc0f3480e7498d4ace99e49319be6a7fed26c21ad43310d2d488fc68ab1c";
        const RTMR3: &str = "bfac8bbe97148d00c0bc5dea273ccd926e2415511f08f5dedaa96d3c19e824d2bf01fae86e8987ff509fd3ad31374a60";
        // Per-hardware RTMR0 (the only register that differs within v1.3.0).
        const RTMR0_H200: &str = "2864b11878e8129095d62a5dd7c3e3aae178d3a077606a825617324768f189ad05aed08376947df92d6c75865d915cbf";
        const RTMR0_RTX_PRO_6000: &str = "5064826bfd530ca9f823ceecb74899d7dbd014b60897a77317a14200c8706f2368ecbbc0a04cec8ceef90474b8c955e1";

        #[test]
        fn covers_the_full_v130_hardware_family() {
            // All five published v1.3.0 hardware platforms are accepted.
            assert_eq!(vetted_golden_measurements().len(), 5);
        }

        #[test]
        fn accepts_rtx_pro_6000_the_config_qwen3_32b_runs_on() {
            // Regression guard: before the v1.3.0-family expansion only 8xh200 was
            // pinned, so a Qwen3-32B-TEE instance scheduled on RTX PRO 6000 hardware
            // — a genuine, signature-verified, nonce-bound quote — was rejected with
            // "observed measurements match no accepted Chutes config". These are its
            // live-observed registers; they must now verify.
            let policy = vetted_golden_measurements();
            let matched = policy
                .verify(
                    &reg(MRTD),
                    &reg(RTMR0_RTX_PRO_6000),
                    &reg(RTMR1),
                    &reg(RTMR2),
                    &reg(RTMR3),
                )
                .expect("RTX PRO 6000 v1.3.0 must be accepted after the family expansion");
            assert_eq!(matched.name, "8xRTX_PRO_6000");
            assert_eq!(matched.version, "1.3.0");
        }

        #[test]
        fn still_accepts_h200_the_original_glm_config() {
            let policy = vetted_golden_measurements();
            let matched = policy
                .verify(
                    &reg(MRTD),
                    &reg(RTMR0_H200),
                    &reg(RTMR1),
                    &reg(RTMR2),
                    &reg(RTMR3),
                )
                .expect("the original 8xh200 v1.3.0 row must keep matching");
            assert_eq!(matched.name, "8xh200");
        }

        #[test]
        fn rejects_v130_software_on_an_unpublished_rtmr0() {
            // Same vetted software identity but a fabricated hardware register that
            // matches no published row — fail-closed, never a soft pass.
            let mut bogus_rtmr0 = reg(RTMR0_H200);
            bogus_rtmr0[0] ^= 0xff;
            let policy = vetted_golden_measurements();
            let err = policy
                .verify(
                    &reg(MRTD),
                    &bogus_rtmr0,
                    &reg(RTMR1),
                    &reg(RTMR2),
                    &reg(RTMR3),
                )
                .unwrap_err();
            assert!(matches!(err, MeasurementError::NoMatch { .. }));
        }
    }
}
