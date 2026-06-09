//! Report-data binding verification, pluggable per provider tier.
//!
//! A TDX quote's 64-byte `report_data` is sealed inside the Intel-signed quote
//! and is where the attestation binds to *this* request and *this* backend:
//!
//! - `report_data[32:64]` = the caller's per-request nonce (freshness).
//! - `report_data[0:32]`  = `SHA256(signing_address ‖ tls_cert_fingerprint)`
//!   when a TLS fingerprint is present, binding the signing key **and** the live
//!   TLS endpoint into the quote.
//!
//! The verifier has historically also accepted a *fallback* for the first 32
//! bytes when no fingerprint was present: `signing_address` zero-padded to 32
//! bytes. That fallback drops the TLS co-binding — an attacker who can set the
//! JSON `signing_address` could satisfy the check against a quote that isn't
//! bound to the TLS endpoint we're actually talking to. [`StrictBoundReportDataVerifier`]
//! removes that fallback and requires the fingerprint binding for **every**
//! attested provider, NEAR's own fleet included: a uniform, no-exceptions bar.
//! NEAR backends already bind the fingerprint (the fallback was never exercised
//! in practice), so this is a hardening, but it must be validated across the
//! whole NEAR fleet in staging before promotion — any model that does not bind
//! a fingerprint would now be (correctly) rejected.

use sha2::{Digest as Sha2Digest, Sha256};

use super::verification::AttestationVerificationError;

/// Verifies the `report_data` binding of a TDX quote. Selected per provider tier
/// at `AttestationVerifier` construction (never from a report field).
pub trait ReportDataVerifier: Send + Sync {
    fn verify(
        &self,
        report_data: &[u8; 64],
        signing_address: &str,
        tls_cert_fingerprint: Option<&str>,
        nonce: &str,
    ) -> Result<(), AttestationVerificationError>;
}

/// `report_data[32:64]` must equal the caller's per-request nonce. Shared by all
/// tiers — freshness is non-negotiable.
fn check_nonce(report_data: &[u8; 64], nonce: &str) -> Result<(), AttestationVerificationError> {
    let nonce_bytes = hex::decode(nonce.strip_prefix("0x").unwrap_or(nonce)).map_err(|e| {
        AttestationVerificationError::InvalidFormat(format!("nonce hex decode: {e}"))
    })?;
    if nonce_bytes.len() != 32 {
        return Err(AttestationVerificationError::ReportDataMismatch(format!(
            "nonce must be 32 bytes, got {}",
            nonce_bytes.len()
        )));
    }
    if report_data[32..64] != nonce_bytes[..] {
        return Err(AttestationVerificationError::ReportDataMismatch(
            "nonce mismatch in report_data[32:64]".to_string(),
        ));
    }
    Ok(())
}

fn decode_addr(signing_address: &str) -> Result<Vec<u8>, AttestationVerificationError> {
    let addr_hex = signing_address
        .strip_prefix("0x")
        .unwrap_or(signing_address);
    hex::decode(addr_hex).map_err(|e| {
        AttestationVerificationError::InvalidFormat(format!("signing_address hex decode: {e}"))
    })
}

/// `report_data[0:32]` must equal `SHA256(signing_address ‖ tls_fingerprint)`.
fn check_fingerprint_binding(
    report_data: &[u8; 64],
    addr_bytes: &[u8],
    fp_hex: &str,
) -> Result<(), AttestationVerificationError> {
    let fp_bytes = hex::decode(fp_hex.strip_prefix("0x").unwrap_or(fp_hex)).map_err(|e| {
        AttestationVerificationError::InvalidFormat(format!("tls_cert_fingerprint hex decode: {e}"))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(addr_bytes);
    hasher.update(&fp_bytes);
    let expected = hasher.finalize();
    if report_data[..32] != expected[..] {
        return Err(AttestationVerificationError::ReportDataMismatch(format!(
            "report_data[0:32] does not match SHA256(signing_address || tls_fingerprint). \
             Expected: {}, got: {}",
            hex::encode(expected),
            hex::encode(&report_data[..32])
        )));
    }
    Ok(())
}

/// The report-data verifier used for **every** attested provider — NEAR's own
/// fleet included. It requires the full binding and admits no exceptions:
///
/// - `report_data[32:64]` == the caller's per-request nonce (freshness), and
/// - `report_data[0:32]` == `SHA256(signing_address ‖ tls_cert_fingerprint)`,
///   so a `tls_cert_fingerprint` is **mandatory** — the legacy zero-padded
///   `signing_address` fallback is gone.
///
/// NEAR backends already bind the TLS fingerprint into `report_data` (the
/// fallback was never exercised in practice), so holding NEAR to this bar is a
/// hardening with no intended behavior change — and it removes the
/// connection-hijack surface that the padded-address fallback opened for any
/// backend we do not operate. Keeping one uniform, no-exceptions rule is the
/// point: the verifier presents the same high bar to NEAR and to third parties.
pub struct StrictBoundReportDataVerifier;

impl ReportDataVerifier for StrictBoundReportDataVerifier {
    fn verify(
        &self,
        report_data: &[u8; 64],
        signing_address: &str,
        tls_cert_fingerprint: Option<&str>,
        nonce: &str,
    ) -> Result<(), AttestationVerificationError> {
        check_nonce(report_data, nonce)?;
        let addr_bytes = decode_addr(signing_address)?;
        let fp_hex = tls_cert_fingerprint.ok_or_else(|| {
            AttestationVerificationError::ReportDataMismatch(
                "attestation report has no tls_cert_fingerprint; the padded signing_address \
                 fallback is not accepted (it would drop the TLS co-binding from report_data)"
                    .to_string(),
            )
        })?;
        check_fingerprint_binding(report_data, &addr_bytes, fp_hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a report_data with the fingerprint binding in [0:32] and nonce in [32:64].
    fn bound_report_data(addr: &[u8], fp: &[u8], nonce: &[u8; 32]) -> [u8; 64] {
        let mut hasher = Sha256::new();
        hasher.update(addr);
        hasher.update(fp);
        let head = hasher.finalize();
        let mut rd = [0u8; 64];
        rd[..32].copy_from_slice(&head);
        rd[32..].copy_from_slice(nonce);
        rd
    }

    const ADDR: &str = "1111111111111111111111111111111111111111";
    const FP: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const NONCE: &str = "3333333333333333333333333333333333333333333333333333333333333333";

    fn nonce_bytes() -> [u8; 32] {
        bytes32(NONCE)
    }
    fn bytes32(h: &str) -> [u8; 32] {
        let v = hex::decode(h).unwrap();
        let mut a = [0u8; 32];
        a.copy_from_slice(&v);
        a
    }

    #[test]
    fn strict_accepts_fingerprint_binding() {
        let rd = bound_report_data(
            &hex::decode(ADDR).unwrap(),
            &hex::decode(FP).unwrap(),
            &nonce_bytes(),
        );
        assert!(StrictBoundReportDataVerifier
            .verify(&rd, ADDR, Some(FP), NONCE)
            .is_ok());
    }

    #[test]
    fn strict_rejects_missing_fingerprint() {
        // Even with a correctly padded address, strict must reject when no fp.
        let mut rd = [0u8; 64];
        let addr = hex::decode(ADDR).unwrap();
        rd[..addr.len()].copy_from_slice(&addr);
        rd[32..].copy_from_slice(&nonce_bytes());
        let err = StrictBoundReportDataVerifier
            .verify(&rd, ADDR, None, NONCE)
            .unwrap_err();
        assert!(format!("{err}").contains("tls_cert_fingerprint"));
    }

    #[test]
    fn strict_rejects_stale_nonce_replay() {
        // Quote bound to NONCE, but verifier challenged with a different nonce.
        let rd = bound_report_data(
            &hex::decode(ADDR).unwrap(),
            &hex::decode(FP).unwrap(),
            &nonce_bytes(),
        );
        let other = "4444444444444444444444444444444444444444444444444444444444444444";
        let err = StrictBoundReportDataVerifier
            .verify(&rd, ADDR, Some(FP), other)
            .unwrap_err();
        assert!(format!("{err}").contains("nonce mismatch"));
    }

    #[test]
    fn rejects_wrong_fingerprint_binding() {
        let rd = bound_report_data(
            &hex::decode(ADDR).unwrap(),
            &hex::decode(FP).unwrap(),
            &nonce_bytes(),
        );
        let wrong_fp = "5555555555555555555555555555555555555555555555555555555555555555";
        assert!(StrictBoundReportDataVerifier
            .verify(&rd, ADDR, Some(wrong_fp), NONCE)
            .is_err());
    }
}
