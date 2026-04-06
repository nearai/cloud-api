//! TLS SPKI fingerprint verification for inference provider connections.
//!
//! Provides a custom rustls `ServerCertVerifier` that wraps the default WebPKI verifier
//! and additionally checks the server certificate's SPKI SHA-256 fingerprint against
//! a dynamically-updatable set of expected fingerprints.
//!
//! Bootstrap mode: when the expected fingerprints set is empty, fingerprint checking
//! is skipped and any valid (WebPKI-verified) certificate is accepted. Once attestation
//! verification populates the set, all new TLS connections are pinned.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// Compute SHA-256 of the SPKI (Subject Public Key Info) DER from an X.509 certificate.
pub fn compute_spki_fingerprint_from_der(cert_der: &[u8]) -> Result<String, String> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der)
        .map_err(|e| format!("failed to parse X.509 DER: {e}"))?;
    let spki_der = cert.tbs_certificate.subject_pki.raw;
    let hash = Sha256::digest(spki_der);
    Ok(hex::encode(hash))
}

/// A TLS certificate verifier that wraps WebPKI verification and additionally
/// checks the server certificate's SPKI SHA-256 fingerprint against an expected set.
///
/// When `expected_fingerprints` is empty, fingerprint checking is skipped (bootstrap mode).
/// When populated, the server cert's SPKI fingerprint must be in the set.
pub struct SpkiFingerprintVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    expected_fingerprints: Arc<RwLock<HashSet<String>>>,
}

impl std::fmt::Debug for SpkiFingerprintVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpkiFingerprintVerifier")
            .field(
                "expected_fingerprints_count",
                &self
                    .expected_fingerprints
                    .read()
                    .map(|s| s.len())
                    .unwrap_or(0),
            )
            .finish()
    }
}

impl SpkiFingerprintVerifier {
    pub fn new(
        inner: Arc<dyn ServerCertVerifier>,
        expected_fingerprints: Arc<RwLock<HashSet<String>>>,
    ) -> Self {
        Self {
            inner,
            expected_fingerprints,
        }
    }
}

impl ServerCertVerifier for SpkiFingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        // First, run the standard WebPKI verification (CA chain, expiry, etc.)
        self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;

        // Check SPKI fingerprint against expected set
        let fps = self
            .expected_fingerprints
            .read()
            .unwrap_or_else(|e| e.into_inner());

        if fps.is_empty() {
            // Bootstrap mode: no fingerprints known yet, accept any valid cert
            return Ok(ServerCertVerified::assertion());
        }

        let spki_hash = compute_spki_fingerprint_from_der(end_entity.as_ref())
            .map_err(|e| TlsError::General(format!("failed to compute SPKI fingerprint: {e}")))?;

        if fps.contains(&spki_hash) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General(format!(
                "TLS certificate SPKI fingerprint {spki_hash} does not match any attested fingerprint"
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Build a `rustls::ClientConfig` using native root certificates and a custom
/// `SpkiFingerprintVerifier` that pins to the given set of expected fingerprints.
pub fn build_rustls_config_with_verifier(
    expected_fingerprints: Arc<RwLock<HashSet<String>>>,
) -> rustls::ClientConfig {
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().expect("failed to load native certs") {
        root_store.add(cert).ok();
    }

    let provider = rustls::crypto::aws_lc_rs::default_provider();

    let default_verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(root_store),
        Arc::new(provider.clone()),
    )
    .build()
    .expect("failed to build WebPKI verifier");

    let verifier = SpkiFingerprintVerifier::new(default_verifier, expected_fingerprints);

    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .expect("failed to set protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();

    // reqwest negotiates ALPN; without this, HTTP/2 and HTTP/1.1 won't work
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_spki_fingerprint_invalid_der() {
        let result = compute_spki_fingerprint_from_der(b"not a cert");
        assert!(result.is_err());
    }
}
