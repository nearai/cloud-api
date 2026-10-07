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
    ChutesMeasurementPolicy, MeasurementError,
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

/// The vetted snapshot of Chutes' golden measurements, compiled in from
/// `chutes_golden_measurements.json` (see [`super::chutes_pins`]).
///
/// Within one software release the hardware rows share MRTD (firmware), RTMR1
/// (kernel), RTMR2 (cmdline/initrd) and the runtime RTMR3 (running app/IMA
/// layer); they differ only in RTMR0, the per-hardware VM-config register. A
/// different release is a distinct identity and its own family.
///
/// Resync rule (since #1192): a row is pinned only if it is published at
/// `GET https://api.chutes.ai/servers/tee/measurements` **and** a quote that
/// passed the Intel signature chain, TCB floor, debug-bit check, report_data
/// bindings and NVIDIA NRAS showed the same five registers. The daily
/// `chutes-measurements-sync` workflow applies this rule and opens a PR; a
/// person reviews and merges it. The feed is unsigned, so being published is
/// never enough on its own. Rows are never removed automatically, and a row
/// whose runtime RTMR3 is all zeros (app unmeasured, v1.0–v1.2) is never
/// pinned.
///
/// Cost of this rule: a published row that no verified quote has shown yet
/// fails closed until it is pinned and released.
///
/// History: #849, #865 and #918 pinned full published families; #1192
/// switched to live-observed rows only and added v1.4.0 / v1.4.1.
pub fn vetted_golden_measurements() -> ChutesMeasurementPolicy {
    super::chutes_pins::PinsFile::compiled().to_policy()
}

#[cfg(test)]
mod tests {
    use super::*;
    use inference_providers::attested::chutes::measurements::ExpectedMeasurement;

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
        const RTMR0_H200_R2: &str = "c0466500b034f7b51be7ea0fcc477e60b54833d927db96e4826ac37c60ec02dc28703a16af551f46be17035157b474da";
        const RTMR0_RTX_PRO_6000: &str = "5064826bfd530ca9f823ceecb74899d7dbd014b60897a77317a14200c8706f2368ecbbc0a04cec8ceef90474b8c955e1";
        const RTMR0_B200: &str = "734628b9a715ec492c2b14b409907f32d91847f439ba8bac2fa985b41c01245536348fefb2e021ed574c290c8c50347a";
        const RTMR0_B200_ETH: &str = "724c1d0d20c11a479d2874fa543b0f1b920be32f2a5b9707fa5bcf6176fff31aeac9436e541e1125f78a0b61f7c2e165";
        const RTMR0_B300: &str = "31f6446add906b7d56132c600549270a8ea780193e0c89586f784b20b25136de441ca715d5ecf86ae72f0b40f7a47f39";

        // Accept the v1.3.0 software identity on `rtmr0`, asserting the matched
        // row is `name`. Drives the full register set through `verify()`.
        fn accepts(rtmr0: &str, name: &str) {
            let policy = vetted_golden_measurements();
            let matched = policy
                .verify(
                    &reg(MRTD),
                    &reg(rtmr0),
                    &reg(RTMR1),
                    &reg(RTMR2),
                    &reg(RTMR3),
                )
                .unwrap_or_else(|e| panic!("{name} v1.3.0 must be accepted: {e}"));
            assert_eq!(matched.name, name);
            assert_eq!(matched.version, "1.3.0");
        }

        #[test]
        fn every_vetted_row_is_well_formed_48_byte_hex() {
            // Guards against a transcription typo (wrong length / non-hex char) in
            // any pinned row — especially the three Blackwell rows whose RTMR0 is
            // only exercised positively by the per-SKU tests below. `verify()`
            // runs `assert_enforceable()` per-request, so an InvalidGolden row
            // would otherwise only surface in production as a fail-closed reject.
            vetted_golden_measurements()
                .assert_enforceable()
                .expect("all pinned golden rows must be valid 48-byte hex");
        }

        #[test]
        fn covers_the_full_v130_hardware_family() {
            // All six published v1.3.0 hardware platforms are accepted — by name,
            // so swapping a row for a different config still fails.
            accepts(RTMR0_H200, "8xh200");
            accepts(RTMR0_H200_R2, "8xh200-r2");
            accepts(RTMR0_RTX_PRO_6000, "8xRTX_PRO_6000");
            accepts(RTMR0_B200, "8xb200");
            accepts(RTMR0_B200_ETH, "8xb200-eth");
            accepts(RTMR0_B300, "8xb300");
        }

        // ── v1.3.1 family (GLM-5.2-TEE) ──────────────────────────────────────
        // v1.3.1-rc1 software identity — GLM-5.2's live fleet (Blackwell only),
        // cross-checked byte-for-byte against signature-verified quotes 2026-06-24.
        const MRTD_V131: &str = "261ce538b435e2d0e85fc97e254bc99154c507b7a8e13d59b69f8532384f1d0bfaadfddf3fccc6e0a411203840bbee8d";
        const RC1_RTMR1: &str = "8cfb5e5a387eef8b5fb7be77ab4405d4b68990d20990e6eec0551c5b682ee7d9fcf7fad7bd6e07b373b2e23321c98a5f";
        const RC1_RTMR2: &str = "2de048a63a3f1ae6bf0f9631bbfa5ffc703392211e2e71dd5fcf645187ee6c4b404883fbbadfcdce294274d9f4ae70ce";
        const RC1_RTMR3: &str = "5b6a2b127a80e4aa71dae6dfa2f1f813e1c1606fdf4ee0947010d29f582813a0860e919860e25febfeb60125988cb9bb";
        const RC1_RTMR0_B200: &str = "7c028a01902475caaa81c245151184d08fcb847cfcbac4ced3c6812d2abe101680d5c015e7cfdb1c98ae54ae7ff0d524";
        const RC1_RTMR0_B200_XEON6: &str = "2b22fa53ace208d4f046ae90b7ad28d71a7f4ef0573897d40f6c82b4036217e3170c856f91e54bc19c20c9958c5d1e36";
        const RC1_RTMR0_B300: &str = "43204fcb166114bee9ea562d88fef18d618f591499f8b73ac87be07962f2b228569b680d2ede4ed719d4a3514f90feda";
        // Final v1.3.1 software identity — same MRTD as -rc1, distinct RTMR1/2/3.
        const FINAL_RTMR1: &str = "9b8b2915351a3166f742024edafb6cce244c1df4056eb1f9eb608c3616b9d63729ae00c98d1dc108009c0978b19dc207";
        const FINAL_RTMR2: &str = "8471360414fe80b4343fb17dd59e442bdc55b5955df0adf610b1de15ad7b454e98fb8e9d38cc188b82369f4f620b6968";
        const FINAL_RTMR3: &str = "51204be641a2af357f5f4e6a121d348d6cb1cbe53c4c35d9dcc3364196b4d41a6e1de75025bb2e76f3b00cc7192f9433";
        const FINAL_RTMR0_H200_1021: &str = "212d8284fe29a52a033cd662763e452915d2002bcc3c3e73aa660b100087bd3cce8aef414c3d7012f6a857f392c1919b";
        // Live-observed rows from the 2026-07-06 incident (genuine, DCAP-signature-
        // verified, nonce-bound prod quotes logged by the fail-closed reject path).
        const FINAL_RTMR0_B200_1021: &str = "35038cbb04f872ac6d2784b05c912c438007583e58960dc66fb02d1b04462dd5994f94536da37b5877ccd3dd27d8d54d";
        const FINAL_RTMR0_H200_1010_FLAT: &str = "ed373dfcc4e3b9cc57282773784c88445699f95705ec0995959c4aa95f9dec454c76da891fa56f820f547b02db8c1f2f";

        fn accepts_family(
            rtmr0: &str,
            mrtd: &str,
            rtmr1: &str,
            rtmr2: &str,
            rtmr3: &str,
            name: &str,
            version: &str,
        ) {
            let policy = vetted_golden_measurements();
            let matched = policy
                .verify(
                    &reg(mrtd),
                    &reg(rtmr0),
                    &reg(rtmr1),
                    &reg(rtmr2),
                    &reg(rtmr3),
                )
                .unwrap_or_else(|e| panic!("{name} v{version} must be accepted: {e}"));
            assert_eq!(matched.name, name);
            assert_eq!(matched.version, version);
        }

        #[test]
        fn accepts_glm52_live_rc1_blackwell_rows() {
            // The three Blackwell register sets observed live on GLM-5.2-TEE's
            // fleet (2026-06-24) — genuine, signature-verified, nonce-bound quotes.
            // Each must verify against the pinned v1.3.1-rc1 family.
            accepts_family(
                RC1_RTMR0_B200,
                MRTD_V131,
                RC1_RTMR1,
                RC1_RTMR2,
                RC1_RTMR3,
                "8xb200",
                "1.3.1-rc1",
            );
            accepts_family(
                RC1_RTMR0_B200_XEON6,
                MRTD_V131,
                RC1_RTMR1,
                RC1_RTMR2,
                RC1_RTMR3,
                "8xb200-xeon6",
                "1.3.1-rc1",
            );
            accepts_family(
                RC1_RTMR0_B300,
                MRTD_V131,
                RC1_RTMR1,
                RC1_RTMR2,
                RC1_RTMR3,
                "8xb300",
                "1.3.1-rc1",
            );
        }

        #[test]
        fn accepts_final_v131_for_rc1_promotion() {
            // Forward-insurance: once Chutes promotes the GLM-5.2 chute off the
            // release candidate, the final v1.3.1 identity (distinct RTMR1/2/3)
            // must already verify so the model does not fail closed again.
            accepts_family(
                FINAL_RTMR0_H200_1021,
                MRTD_V131,
                FINAL_RTMR1,
                FINAL_RTMR2,
                FINAL_RTMR3,
                "8xh200 [10.2.1]",
                "1.3.1",
            );
        }

        #[test]
        fn accepts_the_live_observed_final_v131_rows() {
            // Regression guard for the 2026-07-06 incident: GLM-5.2-TEE (promoted
            // to final v1.3.1 on `8xb200 [10.2.1]`) and GLM-5.1-TEE (on
            // `8xh200 [10.1.0-flat]`) were rejected with "observed measurements
            // match no accepted Chutes config" during a NEAR-fleet outage — the
            // fallback existed but was unusable. These are the live-observed
            // register sets from those signature-verified quotes; they must verify.
            accepts_family(
                FINAL_RTMR0_B200_1021,
                MRTD_V131,
                FINAL_RTMR1,
                FINAL_RTMR2,
                FINAL_RTMR3,
                "8xb200 [10.2.1]",
                "1.3.1",
            );
            accepts_family(
                FINAL_RTMR0_H200_1010_FLAT,
                MRTD_V131,
                FINAL_RTMR1,
                FINAL_RTMR2,
                FINAL_RTMR3,
                "8xh200 [10.1.0-flat]",
                "1.3.1",
            );
        }

        #[test]
        fn accepts_the_2026_08_12_observed_new_v131_hardware_rows() {
            // Chutes' 2026-08-12 transparency-log snapshot added these hardware
            // configurations without changing the final v1.3.1 software identity.
            // Every new RTMR0 must verify with the family's shared MRTD/RTMR1/2/3.
            for (matched_name, rtmr0) in [
                ("8xh200 [10.2.1, FLAT]", "b237753a1c8a05042209947fc0f98c8459783db7f3411860c38249c5abd4efd8c6fb7820036ff19b5099138aaf9e0bd1"),
                ("8xRTX_PRO_6000", "0917443cc41e9a5afebc8e87e69a63f32208c47d4b4b4fd410fbc1a705e1880c1383a4ad51903a5ed20cb4090420185a"),
                ("8xRTX_PRO_6000 [10.2.1, NUMA2-4/4]", "5fc09d108ef74d5505b876690de5ab5da02af463ba84bb33299efd1c02144b5d7a6ba579b3ff31ef9118350468e9faf2"),
                ("8xRTX_PRO_6000 [10.2.1, NUMA2-3/5]", "1de32a41a8116e042f33e9cb813f1f6edfef1452c8b2cf54df5451a848ef8931e97f40927191acabfd111c8b8d66796a"),
                ("8xRTX_PRO_6000 [10.2.1, FLAT]", "e9f0b31ce30e4917767d22ad26ad0a8f4edc095b8d9f4bbb36c9cc24fe274aa2dfe16ce3c961dac2d8cef3e6ae2e901d"),
                ("8xRTX_PRO_6000 [10.2.1, FLAT, MSI-GNR]", "872d965083f0bab7d080bd7d40155ba1b2b911d883f391ab7d6b9d810abeefd058e04129d72c088cf4fd05c099a57704"),
                ("8xb200 [10.2.1, XEON6, 272CPU]", "9673907ceb0c9ca79337437bb91695e7a3d19e82df1e41de1b0d2db8081fccb5d82f26d479a5016553cb20964d5948b9"),
                ("8xb200 [10.2.1, XEON6, SNC3]", "ccef43242ef633a542405dbfe55d04d823a50586b0a07510d058ea88ad8d1f8281f227f00c664435d92b23431c4d1c3c"),
                ("8xb200 [10.2.1, ubuntu3]", "ff42d0f7b03cbe84f9e252d8f912b465852c8ee92584c5c047de134a46c8f1e1545683e713bbdd64e2af7d10e8bafaae"),
            ] {
                accepts_family(
                    rtmr0,
                    MRTD_V131,
                    FINAL_RTMR1,
                    FINAL_RTMR2,
                    FINAL_RTMR3,
                    matched_name,
                    "1.3.1",
                );
            }
        }

        // ── v1.4.0 / v1.4.1 (resync of 2026-10-05) ──────────────────────────
        // Both keep the v1.3.1 MRTD and share RTMR1; RTMR2 and the runtime RTMR3
        // differ per release.
        const V14X_RTMR1: &str = "d3a862ff47357f374fc72c7f02a480a13790d1805e24aaa8de1f03994256625ce0f593ae35ea8f0c24d09f7df36cb0ed";
        const V140_RTMR2: &str = "8feee49b83c0f912f9ab9366a7423f65417bb60223c1871a5d55e84d92d741c80017b3f856317c45295255db70f8dc3e";
        const V140_RTMR3: &str = "7e7adbc834a3e746278f28c607a94ba93407a7b25055daca4836cc6003e9bfb5f48836fa0e1d6ab6637c187dd142e5f6";
        const V141_RTMR2: &str = "da23f73e0fddeb8128f706ecfbecbcf8cee34af7e4907d8fbc85e9b216acee27bf6cc3655eaf4d33cab76adea79fa153";
        const V141_RTMR3: &str = "d9dc4c6079fb12a21ad2aa8e329d8bfa61aaa13d3ffd10a93a2c4e82f0e35efbf28f5cf3bed0c0c1b517a7c327a25226";
        // Observed live under v1.4.1 (and, for this one, published under v1.4.0
        // too, but never observed with the v1.4.0 identity).
        const V14X_RTMR0_B300_FLAT_252C: &str = "fc71a5a8edcb1f6d307d59e62ce257365df0f233ae2334691f99d2136832d911f8be975b92f88036db8a4906574519c4";
        // Published under v1.4.1, never observed in a signature-verified quote.
        const V141_RTMR0_B300_FLAT_188C_UNOBSERVED: &str = "bfaaf9a018fb3f10518b2706731ed010bc7d75f3ea3eabcc562ce4a794ab4ee88eba199e38f9013e96e1099a1cf7c49e";
        // v1.5.0: published, never observed live.
        const V150_RTMR2: &str = "4a93a5f46e53bf95a858ff5556ca3623ffc9780b18f27edfa518bd317a4b4fcf9cf8e1bed1b05e51eba1d9dc846d3e87";
        const V150_RTMR3: &str = "bad2ccec446a52c96bc73f6ae8ff8b2b5b87efaa80333d58b1e5897d5683852e1b09a81c94af16b3ec64e1dba3d26f74";

        #[test]
        fn accepts_the_2026_10_05_live_observed_v131_b300_row() {
            // kimi-k3 instances on `8xb300 [10.2.1, numa-256c]` ran the final
            // v1.3.1 identity but this RTMR0 was missing from the snapshot, so
            // they were rejected. Register set taken from signature-verified
            // production quotes and matched to the published row.
            accepts_family(
                "f24aaec75a6c8783222ab141f003f2180fce573ef9cc6ea3dcdc2fcb91c051650fcd74ec85f02e367f06ed71328d319b",
                MRTD_V131,
                FINAL_RTMR1,
                FINAL_RTMR2,
                FINAL_RTMR3,
                "8xb300 [10.2.1, numa-256c]",
                "1.3.1",
            );
        }

        #[test]
        fn accepts_the_2026_10_05_live_observed_v140_rows() {
            // The two v1.4.0 register sets logged by the fail-closed reject path
            // for signature-verified, nonce-bound production quotes.
            for (matched_name, rtmr0) in [
                ("8xh200 [10.2.1, flat-188c-1128g-nvsw4]", "b72208e4b39593a82bbdbc394569bd6f8395f76bdcff5bce2bc72f4e2ca0b3202238d3b78952b497d725e04999e45435"),
                ("8xh200 [10.2.1, numa-124c-1128g-nvsw-node1]", "5b509103a3bf3c10dbf27a7da030a3d7ba93a81c0c8844de20e5dfee77611644a39cc7236313e9d0a99a8a8a703bbfc9"),
            ] {
                accepts_family(
                    rtmr0,
                    MRTD_V131,
                    V14X_RTMR1,
                    V140_RTMR2,
                    V140_RTMR3,
                    matched_name,
                    "1.4.0",
                );
            }
        }

        #[test]
        fn accepts_the_2026_10_05_live_observed_v141_rows() {
            // Regression guard for the 2026-10-02 kimi-k3 outage: the model's
            // Chutes instances had all moved to `8xb300` rows that were not
            // pinned (these two on v1.4.1, plus the v1.3.1 row above), so every
            // request was refused. The `8xh200` rows are the ones kimi-k2.6,
            // deepseek-v3.2 and qwen3.5-397b were rejected on.
            // All seven register sets come from signature-verified, nonce-bound
            // production quotes and match a published row on all five registers.
            for (matched_name, rtmr0) in [
                ("8xb300 [10.2.1, flat-252c-1944g]", V14X_RTMR0_B300_FLAT_252C),
                ("8xb300 [10.2.1, numa-flatpci-252c-2304g]", "e861504d4a05ba1e949618ef759d6fbf0c69e4774b667e9fb876b664cb1bb70e3094d2cddc9ac6c52d4b5d352ba2e1d3"),
                ("8xh200 [10.2.1, flat-188c-1128g-nvsw4]", "b72208e4b39593a82bbdbc394569bd6f8395f76bdcff5bce2bc72f4e2ca0b3202238d3b78952b497d725e04999e45435"),
                ("8xh200 [10.2.1, numa-124c-1128g-nvsw-node1]", "5b509103a3bf3c10dbf27a7da030a3d7ba93a81c0c8844de20e5dfee77611644a39cc7236313e9d0a99a8a8a703bbfc9"),
                ("8xh200 [10.2.1, numa-188c-1128g-nvsw-node0]", "052183ad6cca5ad4aef7e31dc3f6c47be2ba90c288a55420bf432301374f1bd9932ea7d6b5da999c4fecf578c4b0a21a"),
                ("8xh200 [10.2.1, numa-188c-1128g-nvsw-node1]", "ba81dbf034d968fd4be0975c031cbe6451295d63cb509cd857372fc915dada1b24fc4e1b0486db4a276a9efb49d415e6"),
                ("8xh200 [10.2.1, numa-236c-1128g-nvsw-node1]", "a9cac743d296a96c7a0d6348f03be1e1a407f18ec215f0039e3dc1ea4c132eb1e4a5fe3783022ff3d252d4b30f1dbf9f"),
            ] {
                accepts_family(
                    rtmr0,
                    MRTD_V131,
                    V14X_RTMR1,
                    V141_RTMR2,
                    V141_RTMR3,
                    matched_name,
                    "1.4.1",
                );
            }
        }

        fn rejects(rtmr0: &str, rtmr1: &str, rtmr2: &str, rtmr3: &str) {
            rejects_with(&vetted_golden_measurements(), rtmr0, rtmr1, rtmr2, rtmr3);
        }

        fn rejects_with(
            policy: &ChutesMeasurementPolicy,
            rtmr0: &str,
            rtmr1: &str,
            rtmr2: &str,
            rtmr3: &str,
        ) {
            let err = policy
                .verify(
                    &reg(MRTD_V131),
                    &reg(rtmr0),
                    &reg(rtmr1),
                    &reg(rtmr2),
                    &reg(rtmr3),
                )
                .unwrap_err();
            assert!(matches!(err, MeasurementError::NoMatch { .. }));
        }

        /// The v1.4.x rows pinned by #1192 that the tests below need, as a
        /// fixed fixture. The compiled set only grows (the sync bot adds rows
        /// once they are seen live), so "this published row is not pinned yet"
        /// is tested against this snapshot rather than the compiled file.
        fn v14x_fixture_policy() -> ChutesMeasurementPolicy {
            ChutesMeasurementPolicy::new(vec![
                ExpectedMeasurement::new(
                    "8xh200 [10.2.1, flat-188c-1128g-nvsw4]",
                    "1.4.0",
                    MRTD_V131,
                    "b72208e4b39593a82bbdbc394569bd6f8395f76bdcff5bce2bc72f4e2ca0b3202238d3b78952b497d725e04999e45435",
                    V14X_RTMR1,
                    V140_RTMR2,
                    V140_RTMR3,
                ),
                ExpectedMeasurement::new(
                    "8xb300 [10.2.1, flat-252c-1944g]",
                    "1.4.1",
                    MRTD_V131,
                    V14X_RTMR0_B300_FLAT_252C,
                    V14X_RTMR1,
                    V141_RTMR2,
                    V141_RTMR3,
                ),
            ])
        }

        #[test]
        fn rejects_published_but_unobserved_v14x_rows() {
            // Being in the published feed is not enough: a published row that no
            // verified quote has shown is not pinned, so it must not verify.
            let policy = v14x_fixture_policy();
            // v1.4.1 on a hardware row that was never observed.
            rejects_with(
                &policy,
                V141_RTMR0_B300_FLAT_188C_UNOBSERVED,
                V14X_RTMR1,
                V141_RTMR2,
                V141_RTMR3,
            );
            // An RTMR0 pinned for v1.4.1 does not carry over to v1.4.0, even
            // though Chutes publishes the same hardware row under both.
            rejects_with(
                &policy,
                V14X_RTMR0_B300_FLAT_252C,
                V14X_RTMR1,
                V140_RTMR2,
                V140_RTMR3,
            );
        }

        #[test]
        fn rejects_an_unpinned_software_identity_on_a_pinned_rtmr0() {
            // v1.5.0 (published, same MRTD and RTMR1 as v1.4.x, new RTMR2/3) must
            // not verify on a hardware row pinned only for v1.4.1.
            rejects_with(
                &v14x_fixture_policy(),
                V14X_RTMR0_B300_FLAT_252C,
                V14X_RTMR1,
                V150_RTMR2,
                V150_RTMR3,
            );
        }

        #[test]
        fn rejects_v140_and_v141_identities_stapled_together() {
            // v1.4.0 boot chain (RTMR2) with the v1.4.1 runtime RTMR3 matches no
            // single published row — partial matches are rejected.
            rejects(
                "5b509103a3bf3c10dbf27a7da030a3d7ba93a81c0c8844de20e5dfee77611644a39cc7236313e9d0a99a8a8a703bbfc9",
                V14X_RTMR1,
                V140_RTMR2,
                V141_RTMR3,
            );
        }

        #[test]
        fn rejects_stale_rc1_hardware_carried_onto_final_software() {
            // The previous snapshot carried the -rc1 Blackwell RTMR0s into the
            // final v1.3.1 family on the assumption they'd persist; Chutes instead
            // re-measured them, and never published `8xb200`@7c028a01… for the
            // final release. The stale combination must no longer verify.
            let policy = vetted_golden_measurements();
            let err = policy
                .verify(
                    &reg(MRTD_V131),
                    &reg(RC1_RTMR0_B200),
                    &reg(FINAL_RTMR1),
                    &reg(FINAL_RTMR2),
                    &reg(FINAL_RTMR3),
                )
                .unwrap_err();
            assert!(matches!(err, MeasurementError::NoMatch { .. }));
        }

        #[test]
        fn rejects_rc1_software_with_final_v131_rtmr3() {
            // The -rc1 and final v1.3.1 identities must not cross-match: an -rc1
            // boot/kernel (RTMR1/2) stapled to the final runtime RTMR3 matches no
            // single published row — partial matches are rejected (fail-closed).
            let policy = vetted_golden_measurements();
            let err = policy
                .verify(
                    &reg(MRTD_V131),
                    &reg(RC1_RTMR0_B200),
                    &reg(RC1_RTMR1),
                    &reg(RC1_RTMR2),
                    &reg(FINAL_RTMR3),
                )
                .unwrap_err();
            assert!(matches!(err, MeasurementError::NoMatch { .. }));
        }

        #[test]
        fn accepts_the_three_blackwell_rows() {
            // The b200 / b200-eth / b300 rows have no live signature-verified
            // cross-check yet (documented trade-off); these assert the pinned
            // RTMR0 literals were copied correctly and map to the right SKU, so a
            // typo fails a specific test rather than only the count check.
            accepts(RTMR0_B200, "8xb200");
            accepts(RTMR0_B200_ETH, "8xb200-eth");
            accepts(RTMR0_B300, "8xb300");
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
