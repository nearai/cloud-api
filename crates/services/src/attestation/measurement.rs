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
//! influenced report fields. NEAR's behavior is reproduced exactly by
//! [`MeasurementPolicy::near_from_env`] / [`MeasurementPolicy::near`], so the
//! existing verification path is byte-for-byte unchanged.

use std::collections::HashSet;

use inference_providers::ProviderTier;

use super::verification::AttestationVerificationError;

/// Measurement-verification policy scoped to one provider tier.
///
/// Constructed at pool build time from a known [`ProviderTier`] (PR1), so a
/// Chutes measurement policy can never be applied to a NEAR backend or vice
/// versa. PR2 carries the OS-image-hash allowlist + TCB floor; later PRs extend
/// it with register-pin allowlists for providers (like Chutes) that ship no
/// replayable event log.
#[derive(Clone, Debug)]
pub struct MeasurementPolicy {
    /// Trust tier this policy applies to. Selected at construction, never from a report.
    pub tier: ProviderTier,
    /// Allowed OS image-hash measurements (lowercase hex), checked against the
    /// RTMR3-verified event log's `os-image-hash`.
    pub allowed_os_image_hashes: HashSet<String>,
    /// Reject attestations whose TDX TCB status is not `UpToDate`.
    pub require_tcb_up_to_date: bool,
    /// Require a replayable dstack-shaped RTMR3 event log to authenticate the
    /// measurement (NEAR + preferred Chutes). A register-pin fallback for
    /// providers without an event log is added in a later PR.
    pub require_dstack_event_log: bool,
}

impl MeasurementPolicy {
    /// NEAR's own-fleet policy from explicit values, preserving the exact
    /// semantics of the previous `AttestationVerifier::new` fields: an empty
    /// `allowed_os_image_hashes` **skips** the image-hash check (fail-open is
    /// acceptable for our own fleet).
    pub fn near(allowed_os_image_hashes: HashSet<String>, require_tcb_up_to_date: bool) -> Self {
        Self {
            tier: ProviderTier::Near,
            allowed_os_image_hashes,
            require_tcb_up_to_date,
            require_dstack_event_log: true,
        }
    }

    /// An attested third-party ([`ProviderTier::Attested3p`]) policy. TCB-up-to-date
    /// is enforced by default (stricter than NEAR's own fleet), and an empty
    /// `allowed_os_image_hashes` is rejected by [`Self::assert_enforceable`].
    pub fn attested3p(allowed_os_image_hashes: HashSet<String>) -> Self {
        Self {
            tier: ProviderTier::Attested3p,
            allowed_os_image_hashes,
            require_tcb_up_to_date: true,
            require_dstack_event_log: true,
        }
    }

    /// NEAR's own-fleet policy from environment, reproducing today's behavior:
    /// `ALLOWED_IMAGE_HASHES` (comma-separated, empty = skip) and
    /// `REQUIRE_TCB_UP_TO_DATE`.
    pub fn near_from_env() -> Self {
        let allowed_os_image_hashes: HashSet<String> = std::env::var("ALLOWED_IMAGE_HASHES")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_lowercase())
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

        Self::near(allowed_os_image_hashes, require_tcb_up_to_date)
    }

    /// Fail-closed guard, called before measurement verification.
    ///
    /// For an attested **third party** ([`ProviderTier::Attested3p`]) using the
    /// event-log replay path, an empty allowlist is a misconfiguration that would
    /// otherwise silently accept arbitrary software — so it is rejected here.
    /// For NEAR's own fleet ([`ProviderTier::Near`]) an empty allowlist keeps the
    /// historical skip behavior, and [`ProviderTier::NonAttested`] has no
    /// attestation path, so neither errors.
    pub fn assert_enforceable(&self) -> Result<(), AttestationVerificationError> {
        if self.tier == ProviderTier::Attested3p
            && self.require_dstack_event_log
            && self.allowed_os_image_hashes.is_empty()
        {
            return Err(AttestationVerificationError::ImageHashMismatch(
                "attested third-party provider has no os-image-hash allowlist configured \
                 (fail-closed: empty allowlist would accept arbitrary software)"
                    .to_string(),
            ));
        }
        Ok(())
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
    }

    #[test]
    fn attested3p_empty_allowlist_is_rejected_fail_closed() {
        let p = MeasurementPolicy {
            tier: ProviderTier::Attested3p,
            allowed_os_image_hashes: HashSet::new(),
            require_tcb_up_to_date: true,
            require_dstack_event_log: true,
        };
        assert!(
            p.assert_enforceable().is_err(),
            "attested 3p with empty allowlist must fail closed"
        );
    }

    #[test]
    fn attested3p_nonempty_allowlist_enforced() {
        let p = MeasurementPolicy {
            tier: ProviderTier::Attested3p,
            allowed_os_image_hashes: hashes(&["deadbeef"]),
            require_tcb_up_to_date: true,
            require_dstack_event_log: true,
        };
        assert!(p.assert_enforceable().is_ok());
        assert!(p.enforces_image_hash());
    }
}
