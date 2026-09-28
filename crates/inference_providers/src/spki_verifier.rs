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

    /// Replace the pinned set wholesale.
    ///
    /// Called once per discovery cycle when the cycle achieved complete
    /// coverage (every healthy backend produced exactly one verified
    /// fingerprint). Lets the pin set track the *current* healthy set rather
    /// than accumulating every backend the proxy ever routed to — when a
    /// backend goes unhealthy or its cert rotates, its old fingerprint is
    /// dropped within one refresh interval.
    ///
    /// Transitions Bootstrap → Pinned and Blocked → Pinned, matching
    /// `add_fingerprint`. An empty `fps` is permitted; callers treat that as
    /// "no healthy backends right now" and the provider-level fail-closed
    /// path keeps connections rejected until a future cycle re-pins
    /// something.
    pub fn replace_with(&mut self, fps: HashSet<String>) {
        *self = FingerprintState::Pinned(fps);
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

/// SPKI fingerprint of the leaf certificate the server presented on the
/// connection that carried `resp`.
///
/// The client must be built with `reqwest::ClientBuilder::tls_info(true)`;
/// without it, or on a plain-HTTP response, there is no `TlsInfo` and this
/// returns an error.
pub fn peer_spki_fingerprint(resp: &reqwest::Response) -> Result<String, String> {
    let tls_info = resp
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .ok_or_else(|| "response carries no TLS session information".to_string())?;
    let leaf = tls_info
        .peer_certificate()
        .ok_or_else(|| "TLS session has no peer certificate".to_string())?;
    compute_spki_fingerprint_from_der(leaf)
}

/// Compare a fingerprint computed from a live certificate (lowercase hex, as
/// returned by [`compute_spki_fingerprint_from_der`]) with one taken from an
/// attestation report. The report value is accepted in the same forms the
/// report_data binding check accepts: optional `0x` prefix, either hex case.
pub fn spki_fingerprint_matches(observed: &str, attested: &str) -> bool {
    let attested = attested.strip_prefix("0x").unwrap_or(attested);
    observed.eq_ignore_ascii_case(attested)
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

/// Shared TLS infrastructure (root certs + crypto provider) for building
/// multiple `ClientConfig`s without duplicating the ~150KB root cert store.
#[derive(Clone)]
pub struct SharedTlsRoots {
    root_store: Arc<rustls::RootCertStore>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl SharedTlsRoots {
    /// Load native root certificates once. Reuse via `.clone()` for multiple clients.
    pub fn load() -> Self {
        let mut root_store = rustls::RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        for err in &native.errors {
            tracing::warn!("error loading native root cert: {err}");
        }
        for cert in native.certs {
            root_store.add(cert).ok();
        }
        Self::from_root_store(root_store)
    }

    /// Use a caller-supplied root store instead of the native roots, e.g. a
    /// local test CA.
    pub fn from_root_store(root_store: rustls::RootCertStore) -> Self {
        Self {
            root_store: Arc::new(root_store),
            provider: Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        }
    }

    /// Build a `rustls::ClientConfig` with SPKI fingerprint verification.
    ///
    /// TLS session resumption is disabled. A resumed handshake does not call
    /// `verify_server_cert`, so a session established while the state was
    /// `Bootstrap` would let a later reconnect skip the pin check. Without
    /// resumption, every new connection is checked against the current state.
    /// A full TLS 1.3 handshake takes the same number of round trips as a
    /// resumed one; the added cost is the certificate check.
    pub fn build_config(&self, state: Arc<RwLock<FingerprintState>>) -> rustls::ClientConfig {
        let default_verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
            self.root_store.clone(),
            self.provider.clone(),
        )
        .build()
        .expect("failed to build WebPKI verifier");

        let verifier = SpkiFingerprintVerifier::new(default_verifier, state);

        let mut config = rustls::ClientConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .expect("failed to set protocol versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();

        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        config.resumption = rustls::client::Resumption::disabled();
        config
    }
}

/// Build a `rustls::ClientConfig` using native root certificates and a custom
/// `SpkiFingerprintVerifier` that pins to the given fingerprint state.
///
/// Convenience wrapper — loads root certs each call. For creating many clients,
/// use `SharedTlsRoots::load()` once and call `.build_config()` per client.
pub fn build_rustls_config_with_verifier(
    state: Arc<RwLock<FingerprintState>>,
) -> rustls::ClientConfig {
    SharedTlsRoots::load().build_config(state)
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

    #[test]
    fn test_replace_with_from_bootstrap() {
        let mut state = FingerprintState::Bootstrap;
        let mut fps = HashSet::new();
        fps.insert("a".to_string());
        fps.insert("b".to_string());
        state.replace_with(fps);
        assert!(matches!(state, FingerprintState::Pinned(_)));
        assert_eq!(state.pinned_count(), 2);
    }

    #[test]
    fn test_replace_with_shrinks_pinned() {
        let mut state = FingerprintState::Bootstrap;
        for fp in ["a", "b", "c", "d", "e"] {
            state.add_fingerprint(fp.to_string());
        }
        assert_eq!(state.pinned_count(), 5);

        // Backend went away — pin set tracks the new healthy set.
        let mut shrunk = HashSet::new();
        shrunk.insert("a".to_string());
        shrunk.insert("b".to_string());
        shrunk.insert("c".to_string());
        shrunk.insert("d".to_string());
        state.replace_with(shrunk);
        assert_eq!(state.pinned_count(), 4);
        if let FingerprintState::Pinned(set) = &state {
            assert!(set.contains("a"));
            assert!(!set.contains("e"), "evicted fingerprint must be gone");
        } else {
            panic!("expected Pinned");
        }
    }

    #[test]
    fn test_replace_with_from_blocked() {
        // Blocked → Pinned mirrors add_fingerprint's recovery path.
        let mut state = FingerprintState::Bootstrap;
        state.block();
        assert!(matches!(state, FingerprintState::Blocked));

        let mut fps = HashSet::new();
        fps.insert("recovered".to_string());
        state.replace_with(fps);
        assert!(matches!(state, FingerprintState::Pinned(_)));
        assert_eq!(state.pinned_count(), 1);
    }

    #[test]
    fn test_replace_with_empty_set_is_permitted() {
        // Caller may pass an empty set to express "no healthy backends".
        // The provider-level fail-closed path is responsible for rejecting
        // connections; FingerprintState just stores the (empty) Pinned set.
        let mut state = FingerprintState::Bootstrap;
        state.add_fingerprint("a".to_string());
        state.replace_with(HashSet::new());
        assert!(matches!(state, FingerprintState::Pinned(_)));
        assert_eq!(state.pinned_count(), 0);
    }

    #[test]
    fn spki_fingerprint_matches_accepts_report_encodings() {
        let observed = "ab01cd";
        assert!(spki_fingerprint_matches(observed, "ab01cd"));
        assert!(spki_fingerprint_matches(observed, "AB01CD"));
        assert!(spki_fingerprint_matches(observed, "0xab01cd"));
        assert!(!spki_fingerprint_matches(observed, "ab01ce"));
        assert!(!spki_fingerprint_matches(observed, "ab01"));
        assert!(!spki_fingerprint_matches(observed, ""));
    }

    #[test]
    fn peer_spki_fingerprint_errors_without_tls_info() {
        let resp = reqwest::Response::from(http::Response::new(Vec::<u8>::new()));
        let err = peer_spki_fingerprint(&resp).expect_err("no TlsInfo on this response");
        assert!(err.contains("no TLS session information"), "got: {err}");
    }

    mod tls {
        use super::super::*;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        use rustls::HandshakeKind;
        use std::net::SocketAddr;
        use std::sync::Mutex;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        /// A local CA and one leaf certificate for 127.0.0.1 signed by it.
        struct TestPki {
            roots: SharedTlsRoots,
            leaf: CertificateDer<'static>,
            leaf_key: PrivateKeyDer<'static>,
        }

        fn test_pki() -> TestPki {
            let ca_key = rcgen::KeyPair::generate().unwrap();
            let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
            ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            ca_params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
            let ca_cert = ca_params.self_signed(&ca_key).unwrap();
            let issuer = rcgen::Issuer::new(ca_params, ca_key);

            let leaf_key = rcgen::KeyPair::generate().unwrap();
            let leaf_params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
            let leaf = leaf_params.signed_by(&leaf_key, &issuer).unwrap();

            let mut store = rustls::RootCertStore::empty();
            store.add(ca_cert.der().clone()).unwrap();
            TestPki {
                roots: SharedTlsRoots::from_root_store(store),
                leaf: leaf.der().clone(),
                leaf_key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
            }
        }

        /// TLS server that answers each request with a fixed HTTP/1.1 response
        /// and then closes the connection, so every request opens a new one.
        /// Records the kind of each handshake it completes.
        async fn start_server(pki: &TestPki) -> (SocketAddr, Arc<Mutex<Vec<HandshakeKind>>>) {
            let config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![pki.leaf.clone()], pki.leaf_key.clone_key())
            .unwrap();
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let handshakes = Arc::new(Mutex::new(Vec::new()));
            let seen = handshakes.clone();
            tokio::spawn(async move {
                while let Ok((tcp, _)) = listener.accept().await {
                    let acceptor = acceptor.clone();
                    let seen = seen.clone();
                    tokio::spawn(async move {
                        let Ok(mut tls) = acceptor.accept(tcp).await else {
                            return;
                        };
                        if let Some(kind) = tls.get_ref().1.handshake_kind() {
                            seen.lock().unwrap().push(kind);
                        }
                        let mut head = Vec::new();
                        let mut chunk = [0u8; 1024];
                        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                            match tls.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => head.extend_from_slice(&chunk[..n]),
                            }
                        }
                        let _ = tls
                            .write_all(
                                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                            )
                            .await;
                        let _ = tls.shutdown().await;
                    });
                }
            });
            (addr, handshakes)
        }

        fn client(pki: &TestPki, tls_info: bool) -> reqwest::Client {
            let state = Arc::new(RwLock::new(FingerprintState::Bootstrap));
            reqwest::Client::builder()
                .use_preconfigured_tls(pki.roots.build_config(state))
                .tls_info(tls_info)
                .build()
                .unwrap()
        }

        #[tokio::test]
        async fn peer_spki_fingerprint_reads_the_presented_leaf() {
            let pki = test_pki();
            let (addr, _) = start_server(&pki).await;
            let url = format!("https://{addr}/");
            let expected = compute_spki_fingerprint_from_der(pki.leaf.as_ref()).unwrap();

            let resp = client(&pki, true).get(&url).send().await.unwrap();
            assert_eq!(peer_spki_fingerprint(&resp).unwrap(), expected);

            // Without `tls_info(true)` the certificate is not exposed.
            let resp = client(&pki, false).get(&url).send().await.unwrap();
            assert!(peer_spki_fingerprint(&resp).is_err());
        }

        #[tokio::test]
        async fn reconnects_do_not_resume_the_tls_session() {
            let pki = test_pki();
            let (addr, handshakes) = start_server(&pki).await;
            let url = format!("https://{addr}/");
            let client = client(&pki, true);
            for _ in 0..3 {
                let resp = client.get(&url).send().await.unwrap();
                assert!(resp.status().is_success());
                resp.bytes().await.unwrap();
            }
            let handshakes = handshakes.lock().unwrap().clone();
            assert_eq!(handshakes.len(), 3, "the server closes every connection");
            assert!(
                handshakes.iter().all(|k| *k != HandshakeKind::Resumed),
                "every reconnect must run a full handshake (and the certificate check): {handshakes:?}"
            );
        }
    }
}
