//! Dependency-inversion seam for verifying a Chutes instance.
//!
//! The concrete verifier (`ChutesBackendVerifier`) lives in the `services` crate
//! because it needs DCAP quote verification (`dcap-qvl`) and the shared NVIDIA
//! NRAS GPU path. But the Chutes [`super::Provider`] lives here in
//! `inference_providers` (where the `InferenceProvider` trait is), and `services`
//! depends on `inference_providers`, not the reverse. So the provider depends on
//! this **port** trait, and `services` injects the concrete verifier when it
//! builds the provider in the pool.
//!
//! This keeps the trust-critical verification logic in one audited place
//! (`services`) while letting the provider's data path call it through a narrow,
//! object-safe interface.

use async_trait::async_trait;

use super::evidence::InstanceEvidence;

/// Summary of a successfully verified Chutes instance. Returned to the provider
/// so it can log/annotate which attested config served a request.
#[derive(Debug, Clone)]
pub struct VerifiedInstanceInfo {
    pub instance_id: String,
    /// The attested ML-KEM-768 `e2e_pubkey` (base64) — safe to encapsulate to.
    pub e2e_pubkey: String,
    /// Matched golden config, e.g. `"8xh200 v1.3.0"`.
    pub measurement_config: String,
    /// TDX TCB status (e.g. `"UpToDate"`).
    pub tcb_status: String,
    /// NVIDIA NRAS verdict (e.g. `"PASS"`).
    pub gpu_verdict: String,
}

/// Verifies a Chutes instance's full attestation chain (TDX quote + `report_data`
/// bindings + register-pin measurement + GPU). Implemented by
/// `ChutesBackendVerifier` in the `services` crate.
///
/// `attest_instance` returns `Ok` **only** if `evidence` proves the instance runs
/// vetted software and is bound to `e2e_pubkey` under `boot_nonce`; otherwise an
/// error (whose string is safe to surface — no secrets, no plaintext). The
/// provider must treat any error as fatal and refuse to send inference to that
/// instance — never fall back to an unverified channel.
#[async_trait]
pub trait ChutesInstanceVerifier: Send + Sync {
    async fn attest_instance(
        &self,
        evidence: &InstanceEvidence,
        boot_nonce: &str,
        e2e_pubkey: &str,
    ) -> Result<VerifiedInstanceInfo, String>;
}
