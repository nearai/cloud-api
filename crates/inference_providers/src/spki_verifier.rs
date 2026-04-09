//! TLS SPKI fingerprint verification for inference provider connections.
//!
//! Provides a custom rustls `ServerCertVerifier` that wraps the default WebPKI verifier
//! and additionally checks the server certificate's SPKI SHA-256 fingerprint against
//! a dynamically-updatable set of expected fingerprints.
//!
//! States:
//! - Bootstrap: no fingerprints known yet, accept any valid (WebPKI) cert
//! - Pinned: only accept certs whose SPKI fingerprint is in the verified set
//! - Blocked: attestation failed, reject all connections

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// TLS fingerprint verification state.
#[derive(Debug, Clone)]
pub enum FingerprintState {
    /// Initial state — accept any valid (WebPKI-verified) certificate.
    /// Used during the first attestation fetch before fingerprints are known.
    Bootstrap,
    /// Attestation verified — only accept certificates with these SPKI fingerprints.
    Pinned(HashSet<String>),
    /// Attestation failed — reject all TLS connections.
    Blocked,
}

impl FingerprintState {
    /// Add a verified fingerprint. Transitions Bootstrap → Pinned, or adds to existing Pinned set.
    pub fn add_fingerprint(&mut self, fingerprint: String) {
        match self {
            FingerprintState::Bootstrap => {
                let mut set = HashSet::new();
                set.insert(fingerprint);
                *self = FingerprintState::Pinned(set);
            }
            FingerprintState::Pinned(set) => {
                set.insert(fingerprint);
            }
            FingerprintState::Blocked => {
                // Unblock: attestation succeeded after earlier failure
                let mut set = HashSet::new();
                set.insert(fingerprint);
                *self = FingerprintState::Pinned(set);
            }
        }
    }

    /// Block all connections (attestation failed).
    pub fn block(&mut self) {
        if matches!(self, FingerprintState::Bootstrap) {
            *self = FingerprintState::Blocked;
        }
        // Don't block if already Pinned — keep existing verified fingerprints
    }

    /// Number of pinned fingerprints (0 for Bootstrap/Blocked).
    pub fn pinned_count(&self) -> usize {
        match self {
            FingerprintState::Pinned(set) => set.len(),
            _ => 0,
        }
    }
}

/// Compute SHA-256 of the SPKI (Subject Public Key Info) DER from an X.509 certificate.
pub fn compute_spki_fingerprint_from_der(cert_der: &[u8]) -> Result<String, String> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der)
        .map_err(|e| format!("failed to parse X.509 DER: {e}"))?;
    let spki_der = cert.tbs_certificate.subject_pki.raw;
    let hash = Sha256::digest(spki_der);
    Ok(hex::encode(hash))
}

/// A TLS certificate verifier that wraps WebPKI verification and additionally
/// checks the server certificate's SPKI SHA-256 fingerprint against a typed state.
pub struct SpkiFingerprintVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    state: Arc<RwLock<FingerprintState>>,
}

impl std::fmt::Debug for SpkiFingerprintVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpkiFingerprintVerifier")
            .field(
                "state",
                &self
                    .state
                    .read()
                    .map(|s| format!("{s:?}"))
                    .unwrap_or_default(),
            )
            .finish()
    }
}

impl SpkiFingerprintVerifier {
    pub fn new(inner: Arc<dyn ServerCertVerifier>, state: Arc<RwLock<FingerprintState>>) -> Self {
        Self { inner, state }
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

        let state = self.state.read().unwrap_or_else(|e| e.into_inner());

        match &*state {
            FingerprintState::Bootstrap => Ok(ServerCertVerified::assertion()),
            FingerprintState::Blocked => Err(TlsError::General(
                "TLS connections blocked: attestation verification failed".to_string(),
            )),
            FingerprintState::Pinned(fps) => {
                let spki_hash =
                    compute_spki_fingerprint_from_der(end_entity.as_ref()).map_err(|e| {
                        TlsError::General(format!("failed to compute SPKI fingerprint: {e}"))
                    })?;

                if fps.contains(&spki_hash) {
                    Ok(ServerCertVerified::assertion())
                } else {
                    Err(TlsError::General(format!(
                        "TLS certificate SPKI fingerprint {spki_hash} does not match any attested fingerprint"
                    )))
                }
            }
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
/// `SpkiFingerprintVerifier` that pins to the given fingerprint state.
pub fn build_rustls_config_with_verifier(
    state: Arc<RwLock<FingerprintState>>,
) -> rustls::ClientConfig {
    let mut root_store = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for err in &native.errors {
        tracing::warn!("error loading native root cert: {err}");
    }
    for cert in native.certs {
        root_store.add(cert).ok();
    }

    let provider = rustls::crypto::aws_lc_rs::default_provider();

    let default_verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(root_store),
        Arc::new(provider.clone()),
    )
    .build()
    .expect("failed to build WebPKI verifier");

    let verifier = SpkiFingerprintVerifier::new(default_verifier, state);

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

    #[test]
    fn test_fingerprint_state_transitions() {
        let mut state = FingerprintState::Bootstrap;
        assert_eq!(state.pinned_count(), 0);

        state.add_fingerprint("abc".to_string());
        assert_eq!(state.pinned_count(), 1);
        assert!(matches!(state, FingerprintState::Pinned(_)));

        state.add_fingerprint("def".to_string());
        assert_eq!(state.pinned_count(), 2);

        // Block doesn't override Pinned
        state.block();
        assert_eq!(state.pinned_count(), 2);
    }

    #[test]
    fn test_fingerprint_state_block_from_bootstrap() {
        let mut state = FingerprintState::Bootstrap;
        state.block();
        assert!(matches!(state, FingerprintState::Blocked));
        assert_eq!(state.pinned_count(), 0);

        // Adding a fingerprint unblocks
        state.add_fingerprint("abc".to_string());
        assert!(matches!(state, FingerprintState::Pinned(_)));
        assert_eq!(state.pinned_count(), 1);
    }
}
