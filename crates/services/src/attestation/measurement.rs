//! Per-provider measurement policy for attestation verification.
//!
//! Today the OS-image-hash allowlist is a single global env var
//! (`ALLOWED_IMAGE_HASHES`) that **skips** the check when empty (fail-open) and
//! is shared across every provider. That is acceptable for NEAR's own fleet, but
//! unsafe for a third-party attested provider: an empty allowlist must mean
//! "reject", not "accept anything", and one provider's measurements must never
//! be able to validate another provider's backend.
//!
//! [`MeasurementPolicy`] makes the policy explicit and **per-provider**, selected
//! by [`ProviderTier`] at construction time — never derived from attacker-
//! influenced report fields. Its fields are private: a policy can only be built
//! through the safe constructors below, so it is impossible to construct an
//! attested-tier policy that silently skips its measurement check. NEAR's
//! behavior is reproduced exactly by [`MeasurementPolicy::near_from_env`] /
//! [`MeasurementPolicy::near`].

use std::collections::HashSet;

use inference_providers::ProviderTier;

use super::verification::AttestationVerificationError;

/// Normalize an OS-image-hash for storage/comparison: trim, strip an optional
/// `0x` prefix, lowercase. Keeps allowlist matching robust to formatting.
fn normalize_hash(s: &str) -> String {
    let t = s.trim();
    t.strip_prefix("0x").unwrap_or(t).to_lowercase()
}

/// Measurement-verification policy scoped to one provider tier.
///
/// Constructed at pool build time from a known [`ProviderTier`] (PR1) via the
/// safe constructors only — fields are private — so a Chutes measurement policy
/// can never be applied to a NEAR backend or vice versa, and an attested-tier
/// policy can never be assembled in a fail-open state.
#[derive(Clone, Debug)]
pub struct MeasurementPolicy {
    /// Trust tier this policy applies to. Selected at construction, never from a report.
    tier: ProviderTier,
    /// Allowed OS image-hash measurements (normalized), checked against the
    /// RTMR3-verified event log's `os-image-hash`.
    allowed_os_image_hashes: HashSet<String>,
    /// Reject attestations whose TDX TCB status is not `UpToDate`.
    require_tcb_up_to_date: bool,
    /// Require GPU (NVIDIA NRAS) evidence to be present. When false, a report
    /// with no `nvidia_payload` verifies with `gpu_verdict = None` (best-effort,
    /// for NEAR's non-GPU CVMs). When true, absent GPU evidence is rejected — an
    /// attested third party that claims confidential GPU compute must prove it.
    require_gpu_evidence: bool,
}

impl MeasurementPolicy {
    /// NEAR's own-fleet policy from explicit values, preserving the exact
    /// semantics of the previous `AttestationVerifier::new` fields: an empty
    /// `allowed_os_image_hashes` **skips** the image-hash check (fail-open is
    /// acceptable for our own fleet). Allowlist entries are normalized.
    pub fn near(allowed_os_image_hashes: HashSet<String>, require_tcb_up_to_date: bool) -> Self {
        Self {
            tier: ProviderTier::Near,
            allowed_os_image_hashes: allowed_os_image_hashes
                .iter()
                .map(|s| normalize_hash(s))
                .collect(),
            require_tcb_up_to_date,
            // NEAR's own fleet keeps the historical best-effort GPU behavior
            // (non-GPU CVMs verify with gpu_verdict = None).
            require_gpu_evidence: false,
        }
    }

    /// An attested third-party ([`ProviderTier::Attested3p`]) policy. TCB-up-to-date
    /// is enforced by default (stricter than NEAR's own fleet), and an empty
    /// `allowed_os_image_hashes` is rejected by [`Self::assert_enforceable`].
    /// Allowlist entries are normalized.
    pub fn attested3p(allowed_os_image_hashes: HashSet<String>) -> Self {
        Self {
            tier: ProviderTier::Attested3p,
            allowed_os_image_hashes: allowed_os_image_hashes
                .iter()
                .map(|s| normalize_hash(s))
                .collect(),
            require_tcb_up_to_date: true,
            // A third party advertising confidential GPU compute must present
            // verifiable, nonce-bound GPU evidence — absent is rejected.
            require_gpu_evidence: true,
        }
    }

    /// NEAR's own-fleet policy from environment, reproducing today's behavior:
    /// `ALLOWED_IMAGE_HASHES` (comma-separated, empty = skip) and
    /// `REQUIRE_TCB_UP_TO_DATE`.
    pub fn near_from_env() -> Self {
        let allowed_os_image_hashes: HashSet<String> = std::env::var("ALLOWED_IMAGE_HASHES")
            .unwrap_or_default()
            .split(',')
            .map(normalize_hash)
            .filter(|s| !s.is_empty())
            .collect();

        if !allowed_os_image_hashes.is_empty() {
            tracing::info!(
                count = allowed_os_image_hashes.len(),
                "Loaded allowed image hashes for attestation verification"
            );
        }

        let require_tcb_up_to_date = std::env::var("REQUIRE_TCB_UP_TO_DATE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        // Build directly (not via `near`) so we don't double-normalize; entries
        // are already normalized above.
        Self {
            tier: ProviderTier::Near,
            allowed_os_image_hashes,
            require_tcb_up_to_date,
            require_gpu_evidence: false,
        }
    }

    /// Fail-closed guard, called before measurement verification.
    ///
    /// An attested **third party** ([`ProviderTier::Attested3p`]) with an empty
    /// allowlist is **always** rejected — an empty allowlist would otherwise
    /// silently accept arbitrary software. NEAR's own fleet ([`ProviderTier::Near`])
    /// keeps the historical skip-on-empty behavior, and [`ProviderTier::NonAttested`]
    /// has no attestation path, so neither errors.
    pub fn assert_enforceable(&self) -> Result<(), AttestationVerificationError> {
        if self.tier == ProviderTier::Attested3p && self.allowed_os_image_hashes.is_empty() {
            return Err(AttestationVerificationError::ImageHashMismatch(
                "attested third-party provider has no os-image-hash allowlist configured \
                 (fail-closed: empty allowlist would accept arbitrary software)"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// The trust tier this policy applies to.
    pub fn tier(&self) -> ProviderTier {
        self.tier
    }

    /// Whether attestations with a non-`UpToDate` TCB status must be rejected.
    pub fn require_tcb_up_to_date(&self) -> bool {
        self.require_tcb_up_to_date
    }

    /// Whether GPU evidence must be present (absent GPU evidence is rejected
    /// rather than verifying with `gpu_verdict = None`).
    pub fn require_gpu_evidence(&self) -> bool {
        self.require_gpu_evidence
    }

    /// Whether the OS-image-hash allowlist is enforced for this policy.
    ///
    /// NEAR: only when the allowlist is non-empty (preserving today's
    /// fail-open-on-empty behavior). For an attested third party,
    /// [`Self::assert_enforceable`] has already guaranteed the allowlist is
    /// non-empty, so the check is always enforced.
    pub fn enforces_image_hash(&self) -> bool {
        !self.allowed_os_image_hashes.is_empty()
    }

    /// Whether `hash` (in any case / with-or-without `0x`) is in the allowlist.
    pub fn allows_image_hash(&self, hash: &str) -> bool {
        self.allowed_os_image_hashes.contains(&normalize_hash(hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn near_empty_allowlist_is_enforceable_and_skips() {
        // NEAR's own fleet: empty allowlist keeps the historical skip behavior.
        let p = MeasurementPolicy::near(HashSet::new(), false);
        assert!(p.assert_enforceable().is_ok());
        assert!(!p.enforces_image_hash());
    }

    #[test]
    fn near_nonempty_allowlist_enforces() {
        let p = MeasurementPolicy::near(hashes(&["abc"]), false);
        assert!(p.assert_enforceable().is_ok());
        assert!(p.enforces_image_hash());
        assert!(p.allows_image_hash("abc"));
    }

    #[test]
    fn attested3p_empty_allowlist_is_rejected_fail_closed() {
        // Unconditional: there is no flag a caller can flip to disable this.
        let p = MeasurementPolicy::attested3p(HashSet::new());
        assert!(
            p.assert_enforceable().is_err(),
            "attested 3p with empty allowlist must fail closed"
        );
    }

    #[test]
    fn gpu_evidence_required_only_for_attested3p() {
        assert!(
            !MeasurementPolicy::near(HashSet::new(), false).require_gpu_evidence(),
            "NEAR keeps best-effort GPU (absent allowed)"
        );
        assert!(
            !MeasurementPolicy::near_from_env().require_gpu_evidence(),
            "NEAR-from-env keeps best-effort GPU"
        );
        assert!(
            MeasurementPolicy::attested3p(hashes(&["abc"])).require_gpu_evidence(),
            "attested 3p must present GPU evidence"
        );
    }

    #[test]
    fn attested3p_nonempty_allowlist_enforced_and_tcb_required() {
        let p = MeasurementPolicy::attested3p(hashes(&["deadbeef"]));
        assert!(p.assert_enforceable().is_ok());
        assert!(p.enforces_image_hash());
        assert!(
            p.require_tcb_up_to_date(),
            "attested 3p enforces TCB by default"
        );
    }

    #[test]
    fn allowlist_entries_are_normalized() {
        // Uppercase + 0x-prefixed allowlist entry must still match a lowercase
        // bare hash from the event log (and vice versa).
        let p = MeasurementPolicy::near(hashes(&["0xDEADBEEF"]), false);
        assert!(p.allows_image_hash("deadbeef"));
        assert!(p.allows_image_hash("0xDEADBEEF"));
        assert!(!p.allows_image_hash("cafe"));
    }
}
