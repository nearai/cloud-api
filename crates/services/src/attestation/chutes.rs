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
/// `GET https://api.chutes.ai/servers/tee/measurements`) across the software
/// releases we accept (**v1.3.0** and the **v1.3.1** family) and all of Chutes'
/// GPU hardware platforms.
///
/// Within a single software release the hardware rows are byte-identical on MRTD
/// (firmware), RTMR1 (kernel), RTMR2 (cmdline/initrd) **and the runtime RTMR3**
/// (`runtime_rtmrs.RTMR3`, the running app/IMA layer) — i.e. the full *software*
/// identity is one and the same. They differ **only in RTMR0**, the TD-HOB/ACPI/
/// VM-config layer that varies with the GPU SKU and VM sizing. Pinning each
/// published RTMR0 lets a request land on any of Chutes' vetted hardware
/// platforms while still authenticating the full software stack against a fixed,
/// per-release identity. A *different* software release (v1.3.0 vs v1.3.1 vs
/// v1.3.1-rc1) is a distinct identity (different MRTD and/or RTMR1/2/3) and is
/// listed as its own family below.
///
/// Live cross-checks against genuine, DCAP-signature-verified, report-data-bound
/// quotes (the measurement check runs only *after* the Intel signature chain and
/// the nonce/e2e-key/SPKI bindings pass, so observed registers are trustworthy):
///
/// - `8xh200 v1.3.0` — GLM-5.1-TEE, 6/6 quotes (2026-06-10); and kimi-k2.5 /
///   deepseek-v3.2 / minimax-m2.5 served live over E2EE (2026-06-11).
/// - `8xRTX_PRO_6000 v1.3.0` — Qwen3-32B-TEE, observed register set matched the
///   published row byte-for-byte (2026-06-11).
/// - `8xb200 / 8xb200-xeon6 / 8xb300 v1.3.1-rc1` — **GLM-5.2-TEE**, 3 distinct
///   Blackwell register sets matched the published `1.3.1-rc1` rows byte-for-byte
///   (MRTD + RTMR0 + RTMR1 + RTMR2 + runtime RTMR3), 2026-06-24. GLM-5.2's live
///   fleet runs the `-rc1` firmware; the final `v1.3.1` rows are pinned too so the
///   model keeps attesting once Chutes promotes the chute off the release
///   candidate (the `-rc1` and final identities differ on RTMR1/2/3).
///
/// The remaining rows in each family are taken from the same published release
/// and carry the identical software identity; they are accepted so a model
/// scheduled onto another vetted hardware SKU serves without a per-SKU re-vet.
/// The cryptographic root of trust is the Intel DCAP signature, not the
/// transparency endpoint — these rows merely enumerate which genuine software
/// identities we accept.
///
/// NOT included (fail-closed): the v1.0.0–v1.2.0 rows publish RTMR3 = all-zeros
/// (a boot template, never matchable against a live extended RTMR3 — the running
/// app is unmeasured). Both v1.3.1 and v1.3.1-rc1 publish a non-zero runtime
/// RTMR3, so the app/IMA layer is genuinely measured. Anything not listed here is
/// rejected.
pub fn vetted_golden_measurements() -> ChutesMeasurementPolicy {
    // Each family shares one software identity (MRTD/RTMR1/RTMR2/runtime-RTMR3);
    // only RTMR0 (the per-hardware boot/VM-config register) varies across rows.
    struct Family {
        version: &'static str,
        mrtd: &'static str,
        rtmr1: &'static str,
        rtmr2: &'static str,
        /// runtime RTMR3 (runtime_rtmrs.RTMR3) — the running app/IMA layer.
        rtmr3: &'static str,
        /// (config name, per-hardware RTMR0).
        hardware_rows: &'static [(&'static str, &'static str)],
    }

    const FAMILIES: &[Family] = &[
        // v1.3.0 software identity.
        Family {
            version: "1.3.0",
            mrtd: "ddc6efcdd2309e10837f8a7f64b71272b7ef003b129460410fe715bdfffec38c7c0c1686dddb2a23d4fd623d145e8455",
            rtmr1: "f858ed2aecba4ecd29084352c6b5c6e403c0bec89b8c852f90fa5a8cee796ffa095518c5cd8b92c25c1856e932a95877",
            rtmr2: "7719f4fde518994a5dd6767a8b8b87a38168cc0f3480e7498d4ace99e49319be6a7fed26c21ad43310d2d488fc68ab1c",
            rtmr3: "bfac8bbe97148d00c0bc5dea273ccd926e2415511f08f5dedaa96d3c19e824d2bf01fae86e8987ff509fd3ad31374a60",
            hardware_rows: &[
                ("8xh200", "2864b11878e8129095d62a5dd7c3e3aae178d3a077606a825617324768f189ad05aed08376947df92d6c75865d915cbf"),
                ("8xRTX_PRO_6000", "5064826bfd530ca9f823ceecb74899d7dbd014b60897a77317a14200c8706f2368ecbbc0a04cec8ceef90474b8c955e1"),
                ("8xb200", "734628b9a715ec492c2b14b409907f32d91847f439ba8bac2fa985b41c01245536348fefb2e021ed574c290c8c50347a"),
                ("8xb200-eth", "724c1d0d20c11a479d2874fa543b0f1b920be32f2a5b9707fa5bcf6176fff31aeac9436e541e1125f78a0b61f7c2e165"),
                ("8xb300", "31f6446add906b7d56132c600549270a8ea780193e0c89586f784b20b25136de441ca715d5ecf86ae72f0b40f7a47f39"),
            ],
        },
        // v1.3.1-rc1 software identity — GLM-5.2-TEE's live fleet (Blackwell only,
        // live-verified 2026-06-24).
        Family {
            version: "1.3.1-rc1",
            mrtd: "261ce538b435e2d0e85fc97e254bc99154c507b7a8e13d59b69f8532384f1d0bfaadfddf3fccc6e0a411203840bbee8d",
            rtmr1: "8cfb5e5a387eef8b5fb7be77ab4405d4b68990d20990e6eec0551c5b682ee7d9fcf7fad7bd6e07b373b2e23321c98a5f",
            rtmr2: "2de048a63a3f1ae6bf0f9631bbfa5ffc703392211e2e71dd5fcf645187ee6c4b404883fbbadfcdce294274d9f4ae70ce",
            rtmr3: "5b6a2b127a80e4aa71dae6dfa2f1f813e1c1606fdf4ee0947010d29f582813a0860e919860e25febfeb60125988cb9bb",
            hardware_rows: &[
                ("8xb200", "7c028a01902475caaa81c245151184d08fcb847cfcbac4ced3c6812d2abe101680d5c015e7cfdb1c98ae54ae7ff0d524"),
                ("8xb200-xeon6", "2b22fa53ace208d4f046ae90b7ad28d71a7f4ef0573897d40f6c82b4036217e3170c856f91e54bc19c20c9958c5d1e36"),
                ("8xb300", "43204fcb166114bee9ea562d88fef18d618f591499f8b73ac87be07962f2b228569b680d2ede4ed719d4a3514f90feda"),
            ],
        },
        // Final v1.3.1 software identity — pinned so GLM-5.2 keeps attesting once
        // Chutes promotes the chute off the release candidate. Same MRTD as -rc1,
        // distinct RTMR1/2/3. Published + app-measured; not yet live cross-checked
        // (no instance observed on the final release yet).
        Family {
            version: "1.3.1",
            mrtd: "261ce538b435e2d0e85fc97e254bc99154c507b7a8e13d59b69f8532384f1d0bfaadfddf3fccc6e0a411203840bbee8d",
            rtmr1: "9b8b2915351a3166f742024edafb6cce244c1df4056eb1f9eb608c3616b9d63729ae00c98d1dc108009c0978b19dc207",
            rtmr2: "8471360414fe80b4343fb17dd59e442bdc55b5955df0adf610b1de15ad7b454e98fb8e9d38cc188b82369f4f620b6968",
            rtmr3: "51204be641a2af357f5f4e6a121d348d6cb1cbe53c4c35d9dcc3364196b4d41a6e1de75025bb2e76f3b00cc7192f9433",
            hardware_rows: &[
                ("8xh200 [10.2.1]", "212d8284fe29a52a033cd662763e452915d2002bcc3c3e73aa660b100087bd3cce8aef414c3d7012f6a857f392c1919b"),
                ("8xh200 [10.1.0]", "7e76988fae31dda82f0043b331d908f0716e9da24fed80b6ea6cec9b6615ff84f24321056a8befbea3fff67bd1e59205"),
                ("8xRTX_PRO_6000", "0917443cc41e9a5afebc8e87e69a63f32208c47d4b4b4fd410fbc1a705e1880c1383a4ad51903a5ed20cb4090420185a"),
                ("8xb200", "7c028a01902475caaa81c245151184d08fcb847cfcbac4ced3c6812d2abe101680d5c015e7cfdb1c98ae54ae7ff0d524"),
                ("8xb200-xeon6", "2b22fa53ace208d4f046ae90b7ad28d71a7f4ef0573897d40f6c82b4036217e3170c856f91e54bc19c20c9958c5d1e36"),
                ("8xb300", "43204fcb166114bee9ea562d88fef18d618f591499f8b73ac87be07962f2b228569b680d2ede4ed719d4a3514f90feda"),
            ],
        },
    ];

    ChutesMeasurementPolicy::new(
        FAMILIES
            .iter()
            .flat_map(|f| {
                f.hardware_rows.iter().map(move |&(name, rtmr0)| {
                    ExpectedMeasurement::new(
                        name, f.version, f.mrtd, rtmr0, f.rtmr1, f.rtmr2, f.rtmr3,
                    )
                })
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
            // All five published v1.3.0 hardware platforms are accepted — by name,
            // so swapping a row for a different config (count unchanged) still fails.
            // Total = 5 (v1.3.0) + 3 (v1.3.1-rc1 Blackwell) + 6 (v1.3.1 final).
            assert_eq!(vetted_golden_measurements().len(), 14);
            accepts(RTMR0_H200, "8xh200");
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
