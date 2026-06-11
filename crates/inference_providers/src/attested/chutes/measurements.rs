//! Register-pin verification of a Chutes TDX quote's measurements (MRTD +
//! RTMR0-3) against a vetted snapshot of Chutes' published golden values.
//!
//! Chutes publishes the accepted reference measurements per VM config/version at
//! the public, unauthenticated `GET https://api.chutes.ai/servers/tee/measurements`.
//! Each row has a `boot_rtmrs` set (RTMR3 = 0, pre-runtime) and a **`runtime_rtmrs`**
//! set whose **RTMR3** is the runtime-extended value. We verified (2026-06-10)
//! that live GLM-5.1-TEE quotes match the `8xh200 v1.3.0` row on MRTD, RTMR0,
//! RTMR1, RTMR2 **and** `runtime_rtmrs.RTMR3` byte-for-byte (6/6 live quotes).
//! Because the endpoint is **unsigned** (plain JSON over TLS), we do not fetch it
//! at verify-time; instead a vetted snapshot of the relevant rows is pinned in
//! configuration and checked here.
//!
//! Pinning MRTD + RTMR0-2 fixes the boot chain (firmware/kernel/cmdline);
//! **pinning the runtime RTMR3 additionally fixes the running app/IMA layer** —
//! together this anchors the full software identity, not just the boot chain. A
//! genuine TDX quote running modified software (boot *or* runtime) is rejected.
//!
//! Fail-closed: an empty allow-list, or any malformed golden value, is rejected
//! rather than silently never-matching.

/// SHA-384 register length — MRTD and each RTMR are 48 bytes.
pub const REGISTER_LEN: usize = 48;

/// One accepted configuration: a row from `/servers/tee/measurements`. `mrtd` +
/// `rtmr0..2` are the boot chain (firmware/kernel/cmdline); `rtmr3` is the
/// **runtime** value (from the row's `runtime_rtmrs`, the running app/IMA layer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedMeasurement {
    /// Config name, e.g. `"8xh200"`.
    pub name: String,
    /// VM image version, e.g. `"1.3.0"`.
    pub version: String,
    /// Lowercase hex of the 48-byte MRTD.
    pub mrtd: String,
    /// Lowercase hex of the 48-byte RTMR0.
    pub rtmr0: String,
    /// Lowercase hex of the 48-byte RTMR1.
    pub rtmr1: String,
    /// Lowercase hex of the 48-byte RTMR2.
    pub rtmr2: String,
    /// Lowercase hex of the 48-byte **runtime** RTMR3 (from `runtime_rtmrs`),
    /// the running app/IMA layer.
    pub rtmr3: String,
}

impl ExpectedMeasurement {
    /// Build a config, normalizing each register to lowercase hex without an
    /// optional `0x` prefix (so comparison is canonical regardless of how the
    /// snapshot was transcribed). `rtmr3` is the **runtime** value.
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        mrtd: &str,
        rtmr0: &str,
        rtmr1: &str,
        rtmr2: &str,
        rtmr3: &str,
    ) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            mrtd: norm(mrtd),
            rtmr0: norm(rtmr0),
            rtmr1: norm(rtmr1),
            rtmr2: norm(rtmr2),
            rtmr3: norm(rtmr3),
        }
    }

    fn registers(&self) -> [(&'static str, &str); 5] {
        [
            ("mrtd", &self.mrtd),
            ("rtmr0", &self.rtmr0),
            ("rtmr1", &self.rtmr1),
            ("rtmr2", &self.rtmr2),
            ("rtmr3", &self.rtmr3),
        ]
    }
}

fn norm(s: &str) -> String {
    let t = s.trim();
    t.strip_prefix("0x").unwrap_or(t).to_ascii_lowercase()
}

/// Errors from register-pinning Chutes boot measurements. Every variant is fatal.
#[derive(Debug, thiserror::Error)]
pub enum MeasurementError {
    /// No golden values configured — refuse to attest rather than accept any
    /// measurement (fail-closed; mirrors the NEAR verifier's empty-allowlist
    /// rejection for attested third parties).
    #[error("no accepted Chutes measurements configured — refusing to attest (fail-closed)")]
    EmptyAllowList,
    /// A configured golden value is not valid 48-byte hex (config typo) — reject
    /// at enforcement time rather than silently never-matching.
    #[error("configured golden measurement '{config}' field '{field}' is not valid 48-byte hex")]
    InvalidGolden { config: String, field: &'static str },
    /// The observed registers match no accepted config — the quote is genuine
    /// but is running software (boot or runtime) we have not vetted.
    #[error(
        "observed measurements match no accepted Chutes config \
         (mrtd={mrtd}, rtmr0={rtmr0}, rtmr1={rtmr1}, rtmr2={rtmr2}, rtmr3={rtmr3})"
    )]
    NoMatch {
        mrtd: String,
        rtmr0: String,
        rtmr1: String,
        rtmr2: String,
        rtmr3: String,
    },
}

/// Fail-closed register-pin policy for Chutes boot measurements.
#[derive(Debug, Clone)]
pub struct ChutesMeasurementPolicy {
    allowed: Vec<ExpectedMeasurement>,
}

impl ChutesMeasurementPolicy {
    /// Build from a vetted snapshot of accepted configs.
    pub fn new(allowed: Vec<ExpectedMeasurement>) -> Self {
        Self { allowed }
    }

    /// Number of accepted configs.
    pub fn len(&self) -> usize {
        self.allowed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty()
    }

    /// Reject up-front if the policy could never enforce a real check: an empty
    /// allow-list, or any configured golden value that is not 48-byte hex. Call
    /// this before trusting any quote (the orchestrator does, mirroring
    /// `MeasurementPolicy::assert_enforceable` on the NEAR path).
    pub fn assert_enforceable(&self) -> Result<(), MeasurementError> {
        if self.allowed.is_empty() {
            return Err(MeasurementError::EmptyAllowList);
        }
        for cfg in &self.allowed {
            let label = format!("{} v{}", cfg.name, cfg.version);
            for (field, hexstr) in cfg.registers() {
                match hex::decode(hexstr) {
                    Ok(bytes) if bytes.len() == REGISTER_LEN => {}
                    _ => {
                        return Err(MeasurementError::InvalidGolden {
                            config: label,
                            field,
                        })
                    }
                }
            }
        }
        Ok(())
    }

    /// Verify the observed registers against the accepted configs. On success
    /// returns the matched config (name/version) for logging/audit. **All five**
    /// registers (MRTD + RTMR0-2 boot chain + the runtime RTMR3) must match a
    /// single config — partial matches are rejected.
    pub fn verify(
        &self,
        mrtd: &[u8; REGISTER_LEN],
        rtmr0: &[u8; REGISTER_LEN],
        rtmr1: &[u8; REGISTER_LEN],
        rtmr2: &[u8; REGISTER_LEN],
        rtmr3: &[u8; REGISTER_LEN],
    ) -> Result<&ExpectedMeasurement, MeasurementError> {
        self.assert_enforceable()?;
        let (o_mrtd, o_rtmr0, o_rtmr1, o_rtmr2, o_rtmr3) = (
            hex::encode(mrtd),
            hex::encode(rtmr0),
            hex::encode(rtmr1),
            hex::encode(rtmr2),
            hex::encode(rtmr3),
        );
        self.allowed
            .iter()
            .find(|c| {
                c.mrtd == o_mrtd
                    && c.rtmr0 == o_rtmr0
                    && c.rtmr1 == o_rtmr1
                    && c.rtmr2 == o_rtmr2
                    && c.rtmr3 == o_rtmr3
            })
            .ok_or(MeasurementError::NoMatch {
                mrtd: o_mrtd,
                rtmr0: o_rtmr0,
                rtmr1: o_rtmr1,
                rtmr2: o_rtmr2,
                rtmr3: o_rtmr3,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The live GLM-5.1-TEE measurements, confirmed (2026-06-10) to equal the
    // published `8xh200 v1.3.0` golden row byte-for-byte (MRTD + RTMR0-2 boot
    // chain + `runtime_rtmrs.RTMR3`). PUBLIC transparency-endpoint values.
    const MRTD: &str = "ddc6efcdd2309e10837f8a7f64b71272b7ef003b129460410fe715bdfffec38c7c0c1686dddb2a23d4fd623d145e8455";
    const RTMR0: &str = "2864b11878e8129095d62a5dd7c3e3aae178d3a077606a825617324768f189ad05aed08376947df92d6c75865d915cbf";
    const RTMR1: &str = "f858ed2aecba4ecd29084352c6b5c6e403c0bec89b8c852f90fa5a8cee796ffa095518c5cd8b92c25c1856e932a95877";
    const RTMR2: &str = "7719f4fde518994a5dd6767a8b8b87a38168cc0f3480e7498d4ace99e49319be6a7fed26c21ad43310d2d488fc68ab1c";
    // runtime RTMR3 (runtime_rtmrs.RTMR3) — the running app/IMA layer.
    const RTMR3: &str = "bfac8bbe97148d00c0bc5dea273ccd926e2415511f08f5dedaa96d3c19e824d2bf01fae86e8987ff509fd3ad31374a60";

    fn reg(h: &str) -> [u8; REGISTER_LEN] {
        let v = hex::decode(h).unwrap();
        let mut a = [0u8; REGISTER_LEN];
        a.copy_from_slice(&v);
        a
    }

    fn glm_policy() -> ChutesMeasurementPolicy {
        ChutesMeasurementPolicy::new(vec![ExpectedMeasurement::new(
            "8xh200", "1.3.0", MRTD, RTMR0, RTMR1, RTMR2, RTMR3,
        )])
    }

    #[test]
    fn accepts_matching_full_chain() {
        let p = glm_policy();
        let matched = p
            .verify(
                &reg(MRTD),
                &reg(RTMR0),
                &reg(RTMR1),
                &reg(RTMR2),
                &reg(RTMR3),
            )
            .expect("live GLM-5.1-TEE full chain must match the pinned 8xh200 v1.3.0 row");
        assert_eq!(matched.name, "8xh200");
        assert_eq!(matched.version, "1.3.0");
    }

    #[test]
    fn rejects_any_register_mismatch() {
        let p = glm_policy();
        // A genuine quote whose firmware (RTMR0) differs from any vetted config.
        let mut bad = reg(RTMR0);
        bad[0] ^= 0xff;
        let err = p
            .verify(&reg(MRTD), &bad, &reg(RTMR1), &reg(RTMR2), &reg(RTMR3))
            .unwrap_err();
        assert!(matches!(err, MeasurementError::NoMatch { .. }));
    }

    #[test]
    fn rejects_runtime_rtmr3_mismatch() {
        // Identical boot chain but a different runtime RTMR3 (modified running
        // app/IMA layer) is rejected — the runtime layer is now pinned.
        let p = glm_policy();
        let mut bad = reg(RTMR3);
        bad[0] ^= 0xff;
        let err = p
            .verify(&reg(MRTD), &reg(RTMR0), &reg(RTMR1), &reg(RTMR2), &bad)
            .unwrap_err();
        assert!(matches!(err, MeasurementError::NoMatch { .. }));
    }

    #[test]
    fn empty_allowlist_is_rejected() {
        let p = ChutesMeasurementPolicy::new(vec![]);
        assert!(matches!(
            p.assert_enforceable().unwrap_err(),
            MeasurementError::EmptyAllowList
        ));
        // verify() must also fail closed, never accept.
        assert!(matches!(
            p.verify(
                &reg(MRTD),
                &reg(RTMR0),
                &reg(RTMR1),
                &reg(RTMR2),
                &reg(RTMR3)
            )
            .unwrap_err(),
            MeasurementError::EmptyAllowList
        ));
    }

    #[test]
    fn malformed_golden_is_rejected() {
        let p = ChutesMeasurementPolicy::new(vec![ExpectedMeasurement::new(
            "8xh200", "1.3.0", "not-hex", RTMR0, RTMR1, RTMR2, RTMR3,
        )]);
        assert!(matches!(
            p.assert_enforceable().unwrap_err(),
            MeasurementError::InvalidGolden { field: "mrtd", .. }
        ));
    }

    #[test]
    fn golden_too_short_is_rejected() {
        let p = ChutesMeasurementPolicy::new(vec![ExpectedMeasurement::new(
            "8xh200", "1.3.0", MRTD, RTMR0, RTMR1, RTMR2, "abcd",
        )]);
        assert!(matches!(
            p.assert_enforceable().unwrap_err(),
            MeasurementError::InvalidGolden { field: "rtmr3", .. }
        ));
    }

    #[test]
    fn normalizes_0x_prefix_and_case() {
        // A snapshot transcribed with 0x / uppercase still matches the raw regs.
        let p = ChutesMeasurementPolicy::new(vec![ExpectedMeasurement::new(
            "8xh200",
            "1.3.0",
            &format!("0x{}", MRTD.to_uppercase()),
            RTMR0,
            RTMR1,
            RTMR2,
            RTMR3,
        )]);
        assert!(p
            .verify(
                &reg(MRTD),
                &reg(RTMR0),
                &reg(RTMR1),
                &reg(RTMR2),
                &reg(RTMR3)
            )
            .is_ok());
    }
}
