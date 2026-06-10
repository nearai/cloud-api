//! Chutes-specific `report_data` binding verification.
//!
//! A TDX quote's 64-byte `report_data` is sealed inside the Intel-signed quote;
//! it is where the attestation binds to *this* request and *this* backend.
//! Chutes' layout differs from NEAR's, so it gets its own verifier rather than
//! reusing NEAR's [`StrictBoundReportDataVerifier`]. Both halves were confirmed
//! empirically against the live GLM-5.1-TEE fleet (2026-06-10):
//!
//! - **`report_data[0:32]` = `SHA256(nonce ‖ e2e_pubkey)`** — freshness *and*
//!   E2EE-key binding. The inputs are the **string** forms: the caller's
//!   per-request nonce (the exact hex string used in the `/evidence` query) and
//!   the ML-KEM-768 `e2e_pubkey` (the exact base64 string from
//!   `GET /e2e/instances/{chute}`), UTF-8-concatenated. Mirrors Chutes'
//!   reference verifier `hashlib.sha256((nonce + e2e_pubkey).encode())`.
//!   **Gotcha:** decoding either input to raw bytes does *not* match.
//! - **`report_data[32:64]` = `SHA256(SPKI(certificate))`** — binds the
//!   instance's TLS/identity key (the self-signed attestation cert) into the
//!   quote. Chutes' own reference verifier does not check this half; we do,
//!   because it is what ties the attested key to the endpoint we talk to.
//!
//! This is a pure, fail-closed check: any malformed input or mismatched binding
//! is an error, never a soft pass. The same `SHA256(nonce ‖ e2e_pubkey)` digest
//! is also the nonce the NVIDIA GPU SPDM evidence is bound to, so it is exposed
//! via [`freshness_digest`] for the GPU step to reuse.

use sha2::{Digest, Sha256};

use crate::spki_verifier::compute_spki_fingerprint_from_der;

/// ML-KEM-768 public-key length in bytes. The attested `e2e_pubkey` must be a
/// well-formed key of exactly this size; anything else is rejected before the
/// binding is checked (a degenerate/empty key must never satisfy freshness).
pub const ML_KEM_768_PUBKEY_LEN: usize = 1184;

/// Errors from verifying a Chutes quote's `report_data` bindings. Every variant
/// is fatal — the trust chain is only intact if all bindings hold.
#[derive(Debug, thiserror::Error)]
pub enum ReportDataError {
    /// The caller nonce is not 32 bytes of hex (we generate it; the freshness
    /// anchor must be a full-entropy 32-byte value).
    #[error("nonce must be 32 bytes of hex (64 hex chars): {0}")]
    NonceFormat(String),
    /// The `e2e_pubkey` is not a valid base64 ML-KEM-768 key.
    #[error("e2e_pubkey is not a valid base64 ML-KEM-768 public key: {0}")]
    E2eePubkeyFormat(String),
    /// Computing `SHA256(SPKI(..))` from the instance certificate failed.
    #[error("could not compute SPKI fingerprint from instance certificate: {0}")]
    Spki(String),
    /// `report_data[0:32] != SHA256(nonce ‖ e2e_pubkey)` — the quote is stale /
    /// replayed, or is bound to a different E2EE key than the one presented.
    #[error(
        "report_data[0:32] does not match SHA256(nonce ‖ e2e_pubkey) — stale/replayed quote or \
         wrong E2EE key (expected {expected}, got {got})"
    )]
    FreshnessMismatch { expected: String, got: String },
    /// `report_data[32:64] != SHA256(SPKI(cert))` — the quote is not bound to
    /// this instance's TLS/identity key.
    #[error(
        "report_data[32:64] does not match SHA256(SPKI(certificate)) — quote not bound to this \
         instance's key (expected {expected}, got {got})"
    )]
    TlsBindingMismatch { expected: String, got: String },
}

/// `SHA256(nonce ‖ e2e_pubkey)` over the **verbatim string** forms — the value
/// embedded in `report_data[0:32]` *and* the nonce the GPU SPDM evidence is
/// bound to. The caller must pass the exact strings used on the wire (the
/// `/evidence` query nonce and the `/e2e/instances` base64 key).
pub fn freshness_digest(nonce: &str, e2e_pubkey: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(nonce.as_bytes());
    hasher.update(e2e_pubkey.as_bytes());
    hasher.finalize().into()
}

/// Verifies both `report_data` bindings of a Chutes TDX quote. Fail-closed.
pub struct ChutesReportDataVerifier;

impl ChutesReportDataVerifier {
    /// Verify a Chutes quote's `report_data`:
    ///
    /// - `report_data` — the quote's 64-byte `report_data` field.
    /// - `nonce` — the exact per-request nonce string used in the `/evidence`
    ///   query (we generate it; must be 32 bytes of hex).
    /// - `e2e_pubkey` — the exact base64 ML-KEM-768 key string from
    ///   `/e2e/instances` for this instance.
    /// - `cert_der` — the instance's certificate as raw DER (already
    ///   base64-decoded from `/evidence`).
    pub fn verify(
        &self,
        report_data: &[u8; 64],
        nonce: &str,
        e2e_pubkey: &str,
        cert_der: &[u8],
    ) -> Result<(), ReportDataError> {
        // 1. The nonce is *our* freshness anchor — require a full 32-byte hex
        //    value. (Hashed verbatim below; this only validates entropy/format.)
        let nonce_bytes = hex::decode(nonce.strip_prefix("0x").unwrap_or(nonce))
            .map_err(|e| ReportDataError::NonceFormat(format!("hex decode: {e}")))?;
        if nonce_bytes.len() != 32 {
            return Err(ReportDataError::NonceFormat(format!(
                "got {} bytes, want 32",
                nonce_bytes.len()
            )));
        }

        // 2. The attested key must be a well-formed ML-KEM-768 key. (Decoded
        //    only to validate; the binding hashes the verbatim base64 string.)
        let decoded = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(e2e_pubkey.trim())
                .map_err(|e| ReportDataError::E2eePubkeyFormat(format!("base64: {e}")))?
        };
        if decoded.len() != ML_KEM_768_PUBKEY_LEN {
            return Err(ReportDataError::E2eePubkeyFormat(format!(
                "decoded length {} != {ML_KEM_768_PUBKEY_LEN}",
                decoded.len()
            )));
        }

        // 3. Freshness + E2EE-key binding: report_data[0:32].
        let expected_head = freshness_digest(nonce, e2e_pubkey);
        if report_data[..32] != expected_head[..] {
            return Err(ReportDataError::FreshnessMismatch {
                expected: hex::encode(expected_head),
                got: hex::encode(&report_data[..32]),
            });
        }

        // 4. TLS/identity binding: report_data[32:64] == SHA256(SPKI(cert)).
        let spki_fp = compute_spki_fingerprint_from_der(cert_der).map_err(ReportDataError::Spki)?;
        let got_tail = hex::encode(&report_data[32..64]);
        if spki_fp != got_tail {
            return Err(ReportDataError::TlsBindingMismatch {
                expected: spki_fp,
                got: got_tail,
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    // The synthetic self-signed fixture reused from the transform tests. Its SPKI
    // fingerprint is pinned (independently re-derived via openssl), so a regression
    // in `compute_spki_fingerprint_from_der` is caught here too.
    const CERT_B64: &str = include_str!("testdata/synthetic_cert.b64");
    const CERT_SPKI_FP: &str = "e7c25815d0d940fea893d56984e131788afa6e931920093c9c2896fb04dea0da";
    const NONCE: &str = "344a44ebef795bb19d3caaf4470607e48f219a01652f3603913768e4f75379af";

    fn cert_der() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(CERT_B64.trim())
            .unwrap()
    }

    // A syntactically valid ML-KEM-768 key (right length; content irrelevant to
    // the string-form binding, but the length gate must pass).
    fn pubkey_b64() -> String {
        base64::engine::general_purpose::STANDARD.encode(vec![7u8; ML_KEM_768_PUBKEY_LEN])
    }

    // Build a report_data that satisfies both bindings for the given inputs.
    fn bound_report_data(nonce: &str, pubkey: &str, cert_der: &[u8]) -> [u8; 64] {
        let mut rd = [0u8; 64];
        rd[..32].copy_from_slice(&freshness_digest(nonce, pubkey));
        let fp = compute_spki_fingerprint_from_der(cert_der).unwrap();
        rd[32..].copy_from_slice(&hex::decode(fp).unwrap());
        rd
    }

    #[test]
    fn accepts_correct_bindings() {
        let pk = pubkey_b64();
        let der = cert_der();
        let rd = bound_report_data(NONCE, &pk, &der);
        assert!(ChutesReportDataVerifier
            .verify(&rd, NONCE, &pk, &der)
            .is_ok());
        // sanity: the pinned fp is what landed in [32:64].
        assert_eq!(hex::encode(&rd[32..]), CERT_SPKI_FP);
    }

    #[test]
    fn rejects_stale_nonce_replay() {
        // Quote bound to NONCE, verifier challenged with a different fresh nonce.
        let pk = pubkey_b64();
        let der = cert_der();
        let rd = bound_report_data(NONCE, &pk, &der);
        let other = "4444444444444444444444444444444444444444444444444444444444444444";
        let err = ChutesReportDataVerifier
            .verify(&rd, other, &pk, &der)
            .unwrap_err();
        assert!(matches!(err, ReportDataError::FreshnessMismatch { .. }));
    }

    #[test]
    fn rejects_wrong_e2e_pubkey() {
        // Bound to one key, presented with a different (well-formed) key.
        let der = cert_der();
        let rd = bound_report_data(NONCE, &pubkey_b64(), &der);
        let other_pk =
            base64::engine::general_purpose::STANDARD.encode(vec![9u8; ML_KEM_768_PUBKEY_LEN]);
        let err = ChutesReportDataVerifier
            .verify(&rd, NONCE, &other_pk, &der)
            .unwrap_err();
        assert!(matches!(err, ReportDataError::FreshnessMismatch { .. }));
    }

    #[test]
    fn rejects_wrong_certificate_binding() {
        // [32:64] holds a different SPKI than the presented cert's.
        let pk = pubkey_b64();
        let der = cert_der();
        let mut rd = bound_report_data(NONCE, &pk, &der);
        rd[32] ^= 0xff; // corrupt the SPKI binding
        let err = ChutesReportDataVerifier
            .verify(&rd, NONCE, &pk, &der)
            .unwrap_err();
        assert!(matches!(err, ReportDataError::TlsBindingMismatch { .. }));
    }

    #[test]
    fn rejects_malformed_nonce() {
        let pk = pubkey_b64();
        let der = cert_der();
        let rd = bound_report_data(NONCE, &pk, &der);
        // too short
        assert!(matches!(
            ChutesReportDataVerifier
                .verify(&rd, "abcd", &pk, &der)
                .unwrap_err(),
            ReportDataError::NonceFormat(_)
        ));
        // non-hex
        assert!(matches!(
            ChutesReportDataVerifier
                .verify(&rd, &"z".repeat(64), &pk, &der)
                .unwrap_err(),
            ReportDataError::NonceFormat(_)
        ));
    }

    #[test]
    fn rejects_wrong_length_pubkey() {
        let der = cert_der();
        let short_pk = base64::engine::general_purpose::STANDARD.encode(vec![1u8; 32]);
        // report_data here is irrelevant; the length gate fires first.
        let rd = [0u8; 64];
        assert!(matches!(
            ChutesReportDataVerifier
                .verify(&rd, NONCE, &short_pk, &der)
                .unwrap_err(),
            ReportDataError::E2eePubkeyFormat(_)
        ));
    }

    #[test]
    fn rejects_invalid_certificate() {
        // [0:32] must satisfy freshness so we reach the cert (step 4) — then a
        // non-DER cert makes SPKI computation fail closed.
        let pk = pubkey_b64();
        let mut rd = [0u8; 64];
        rd[..32].copy_from_slice(&freshness_digest(NONCE, &pk));
        assert!(matches!(
            ChutesReportDataVerifier
                .verify(&rd, NONCE, &pk, b"not a der cert")
                .unwrap_err(),
            ReportDataError::Spki(_)
        ));
    }

    #[test]
    fn freshness_digest_uses_string_forms_not_decoded_bytes() {
        // Regression guard for the load-bearing gotcha: the preimage is the ASCII
        // concat of the hex nonce + base64 pubkey, NOT their decoded bytes.
        let pk = pubkey_b64();
        let string_form = freshness_digest(NONCE, &pk);
        let mut decoded_form = Sha256::new();
        decoded_form.update(hex::decode(NONCE).unwrap());
        decoded_form.update(
            base64::engine::general_purpose::STANDARD
                .decode(&pk)
                .unwrap(),
        );
        let decoded_form: [u8; 32] = decoded_form.finalize().into();
        assert_ne!(string_form, decoded_form);
    }
}
