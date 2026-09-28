//! Channel binding on the inline verification path
//! (`PoolBackendVerifier::create_verified_client`, slow path).
//!
//! Two local TLS backends present certificates for 127.0.0.1 from the same
//! test CA: "genuine" and "relay". Both serve an attestation report whose TLS
//! fingerprint is the genuine backend's, as a relay forwarding the genuine
//! report would. Most tests use a stub verifier that accepts the report and
//! returns the fingerprint it carries, so they exercise the connection
//! handling only; `QuoteBoundVerifier` additionally runs the production
//! report_data check. Expected: the relay is refused and nothing is pinned;
//! the genuine backend is pinned and the returned client keeps serving on the
//! connection that carried the report.

use super::*;
use crate::attestation::{
    AttestationVerificationError, ReportDataVerifier, StrictBoundReportDataVerifier,
    VerifiedAttestation,
};
use crate::metrics::capturing::CapturingMetricsService;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use inference_providers::spki_verifier::compute_spki_fingerprint_from_der;
use inference_providers::{BackendVerifier as _, BackendVerifyError};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::TcpListener;

/// Signing address in the test reports.
const SIGNING_ADDRESS: &str = "0x00000000000000000000000000000000000000aa";

/// Stand-in for the TDX / GPU verification: accepts any report and returns
/// the TLS fingerprint it carries, as `AttestationVerifier` does once the
/// quote's report_data binding has been checked.
struct AcceptReportVerifier;

#[async_trait::async_trait]
impl BackendAttestationVerifier for AcceptReportVerifier {
    async fn verify_attestation_report(
        &self,
        attestation_report: &serde_json::Map<String, serde_json::Value>,
        _request_nonce: &str,
    ) -> Result<VerifiedAttestation, AttestationVerificationError> {
        Ok(verified(
            attestation_report
                .get("tls_cert_fingerprint")
                .and_then(|v| v.as_str()),
        ))
    }
}

/// Accepts any report but attests no TLS fingerprint.
struct NoFingerprintVerifier;

#[async_trait::async_trait]
impl BackendAttestationVerifier for NoFingerprintVerifier {
    async fn verify_attestation_report(
        &self,
        _attestation_report: &serde_json::Map<String, serde_json::Value>,
        _request_nonce: &str,
    ) -> Result<VerifiedAttestation, AttestationVerificationError> {
        Ok(verified(None))
    }
}

/// Runs the production report_data check against a quote whose report_data
/// binds the genuine backend's key and the request nonce: the quote a relay
/// obtains by forwarding the nonce to the genuine backend. The verified
/// fingerprint is the report value that passed that check, as in
/// `AttestationVerifier::verify_attestation_report`.
struct QuoteBoundVerifier {
    genuine_fingerprint: String,
}

#[async_trait::async_trait]
impl BackendAttestationVerifier for QuoteBoundVerifier {
    async fn verify_attestation_report(
        &self,
        report: &serde_json::Map<String, serde_json::Value>,
        request_nonce: &str,
    ) -> Result<VerifiedAttestation, AttestationVerificationError> {
        let mut report_data = [0u8; 64];
        let mut binding = Sha256::new();
        binding.update(hex::decode(SIGNING_ADDRESS.trim_start_matches("0x")).unwrap());
        binding.update(hex::decode(&self.genuine_fingerprint).unwrap());
        report_data[..32].copy_from_slice(&binding.finalize());
        report_data[32..].copy_from_slice(&hex::decode(request_nonce).unwrap());

        let address = report
            .get("signing_address")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AttestationVerificationError::MissingField("signing_address".into()))?;
        let fingerprint = report.get("tls_cert_fingerprint").and_then(|v| v.as_str());
        StrictBoundReportDataVerifier.verify(&report_data, address, fingerprint, request_nonce)?;
        Ok(verified(fingerprint))
    }
}

/// Returns a fixed verified fingerprint whatever the report says.
struct FixedFingerprintVerifier {
    fingerprint: String,
}

#[async_trait::async_trait]
impl BackendAttestationVerifier for FixedFingerprintVerifier {
    async fn verify_attestation_report(
        &self,
        _attestation_report: &serde_json::Map<String, serde_json::Value>,
        _request_nonce: &str,
    ) -> Result<VerifiedAttestation, AttestationVerificationError> {
        Ok(verified(Some(&self.fingerprint)))
    }
}

fn verified(tls_cert_fingerprint: Option<&str>) -> VerifiedAttestation {
    VerifiedAttestation {
        tls_cert_fingerprint: tls_cert_fingerprint.map(str::to_string),
        signing_address: SIGNING_ADDRESS.to_string(),
        tcb_status: "UpToDate".to_string(),
        advisory_ids: Vec::new(),
        os_image_hash: None,
        compose_hash: None,
        gpu_verdict: None,
    }
}

struct Leaf {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    fingerprint: String,
}

struct Pki {
    roots: SharedTlsRoots,
    genuine: Leaf,
    relay: Leaf,
}

/// A test CA (trusted through `SharedTlsRoots::from_root_store`) and two leaf
/// certificates for 127.0.0.1 with distinct keys.
fn pki() -> Pki {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca_params, ca_key);

    let leaf = || {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
            .unwrap()
            .signed_by(&key, &issuer)
            .unwrap();
        Leaf {
            fingerprint: compute_spki_fingerprint_from_der(cert.der()).unwrap(),
            cert: cert.der().clone(),
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        }
    };
    let genuine = leaf();
    let relay = leaf();
    assert_ne!(genuine.fingerprint, relay.fingerprint);

    let mut store = rustls::RootCertStore::empty();
    store.add(ca_cert.der().clone()).unwrap();
    Pki {
        roots: SharedTlsRoots::from_root_store(store),
        genuine,
        relay,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    /// TLS; the server offers `h2` and `http/1.1`, so HTTP/2 is negotiated.
    TlsH2,
    /// TLS; the server offers `http/1.1` only.
    TlsHttp1,
    /// Plain HTTP/1.1 without TLS.
    PlainHttp1,
}

fn tls_acceptor(leaf: &Leaf, transport: Transport) -> Option<tokio_rustls::TlsAcceptor> {
    let alpn = match transport {
        Transport::PlainHttp1 => return None,
        Transport::TlsH2 => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        Transport::TlsHttp1 => vec![b"http/1.1".to_vec()],
    };
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![leaf.cert.clone()], leaf.key.clone_key())
    .unwrap();
    config.alpn_protocols = alpn;
    Some(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

#[derive(Clone, Default)]
struct Counters {
    /// Completed TLS handshakes (accepted TCP connections for `PlainHttp1`).
    connections: Arc<AtomicUsize>,
    attestation_requests: Arc<AtomicUsize>,
    /// Attestation requests that carried an `Authorization` header.
    attestation_requests_with_auth: Arc<AtomicUsize>,
    completion_requests: Arc<AtomicUsize>,
}

fn count(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::SeqCst)
}

struct Backend {
    base_url: String,
    counters: Counters,
}

/// How a test backend answers.
struct Behavior {
    name: &'static str,
    /// JSON served at `/v1/attestation/report`.
    report: String,
    /// Answer the attestation request with a 307 to the same path and query
    /// under this base URL.
    redirect_to: Option<String>,
    /// Send `connection: close` on every (HTTP/1.1) response, so the client
    /// cannot reuse a connection.
    close_connections: bool,
}

/// Report JSON attesting `fingerprint`.
fn report(fingerprint: &str, signing_address: &str) -> serde_json::Value {
    serde_json::json!({
        "tls_cert_fingerprint": fingerprint,
        "signing_address": signing_address,
        "intel_quote": "00",
    })
}

async fn handle(
    behavior: Arc<Behavior>,
    counters: Counters,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_default();
    let has_auth = req.headers().contains_key(hyper::header::AUTHORIZATION);
    let _ = req.into_body().collect().await;
    let mut response = Response::builder();
    if behavior.close_connections {
        response = response.header("connection", "close");
    }
    let (status, body) = match (method, path.as_str()) {
        (Method::GET, "/v1/attestation/report") => {
            counters.attestation_requests.fetch_add(1, Ordering::SeqCst);
            if has_auth {
                counters
                    .attestation_requests_with_auth
                    .fetch_add(1, Ordering::SeqCst);
            }
            if let Some(target) = &behavior.redirect_to {
                return Ok(response
                    .status(StatusCode::TEMPORARY_REDIRECT)
                    .header("location", format!("{target}{path_and_query}"))
                    .body(Full::new(Bytes::new()))
                    .unwrap());
            }
            (StatusCode::OK, behavior.report.clone())
        }
        (Method::GET, "/v1/models") => {
            (StatusCode::OK, r#"{"object":"list","data":[]}"#.to_string())
        }
        (Method::POST, "/v1/chat/completions") => {
            counters.completion_requests.fetch_add(1, Ordering::SeqCst);
            (
                StatusCode::OK,
                serde_json::json!({ "served_by": behavior.name }).to_string(),
            )
        }
        _ => (StatusCode::NOT_FOUND, "{}".to_string()),
    };
    Ok(response
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

/// Start a backend that serves `/v1/models`, `/v1/chat/completions` and an
/// attestation report attesting `attested_fingerprint`, presenting `leaf`
/// over TLS.
async fn start_backend(
    name: &'static str,
    leaf: &Leaf,
    transport: Transport,
    attested_fingerprint: &str,
) -> Backend {
    start_backend_with(
        leaf,
        transport,
        Behavior {
            name,
            report: report(attested_fingerprint, SIGNING_ADDRESS).to_string(),
            redirect_to: None,
            close_connections: false,
        },
    )
    .await
}

async fn start_backend_with(leaf: &Leaf, transport: Transport, behavior: Behavior) -> Backend {
    let acceptor = tls_acceptor(leaf, transport);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = match acceptor {
        Some(_) => format!("https://{addr}"),
        None => format!("http://{addr}"),
    };
    let counters = Counters::default();
    let behavior = Arc::new(behavior);
    let server_counters = counters.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let counters = server_counters.clone();
            let behavior = behavior.clone();
            tokio::spawn(async move {
                let connections = counters.connections.clone();
                let service =
                    service_fn(move |req| handle(behavior.clone(), counters.clone(), req));
                let Some(acceptor) = acceptor else {
                    connections.fetch_add(1, Ordering::SeqCst);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tcp), service)
                        .await;
                    return;
                };
                // A handshake the client aborts (certificate rejected by its
                // verifier) fails here and is not counted.
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                connections.fetch_add(1, Ordering::SeqCst);
                if tls.get_ref().1.alpn_protocol() == Some(b"h2".as_slice()) {
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                } else {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                }
            });
        }
    });
    Backend { base_url, counters }
}

fn pool_verifier(
    pki: &Pki,
    state: Arc<std::sync::RwLock<FingerprintState>>,
    metrics: Arc<CapturingMetricsService>,
) -> PoolBackendVerifier {
    PoolBackendVerifier {
        // Set so the tests can check it is not sent on the attestation fetch.
        api_key: Some("backend-token".to_string()),
        model_name: "test-model".to_string(),
        tls_roots: pki.roots.clone(),
        attestation_verifier: Arc::new(AcceptReportVerifier),
        fingerprint_state: state,
        metrics_service: Arc::new(std::sync::OnceLock::from(
            metrics as Arc<dyn crate::metrics::MetricsServiceTrait>,
        )),
    }
}

fn pool_verifier_with(
    pki: &Pki,
    state: Arc<std::sync::RwLock<FingerprintState>>,
    metrics: Arc<CapturingMetricsService>,
    attestation_verifier: Arc<dyn BackendAttestationVerifier>,
) -> PoolBackendVerifier {
    PoolBackendVerifier {
        attestation_verifier,
        ..pool_verifier(pki, state, metrics)
    }
}

/// Message of a channel-binding failure; panics on any other result.
fn channel_binding_error(result: Result<reqwest::Client, BackendVerifyError>) -> String {
    match result {
        Err(BackendVerifyError::ChannelBinding(message)) => message,
        other => panic!("expected a channel-binding failure, got {other:?}"),
    }
}

/// Message of any other verification failure; panics on any other result.
fn other_error(result: Result<reqwest::Client, BackendVerifyError>) -> String {
    match result {
        Err(BackendVerifyError::Other(message)) => message,
        other => panic!("expected a verification failure, got {other:?}"),
    }
}

/// `result:` tags of the recorded channel-binding counters.
fn channel_binding_results(metrics: &CapturingMetricsService) -> Vec<String> {
    metrics
        .get_metrics()
        .into_iter()
        .filter(|m| m.name == crate::metrics::consts::METRIC_BACKEND_CHANNEL_BINDING)
        .map(|m| {
            assert!(m.tags.contains(&"model:test-model".to_string()), "{m:?}");
            assert!(m.tags.contains(&"path:inline_verify".to_string()), "{m:?}");
            m.tags
                .into_iter()
                .find(|t| t.starts_with("result:"))
                .unwrap()
        })
        .collect()
}

fn pinned_set(state: &std::sync::RwLock<FingerprintState>) -> Option<HashSet<String>> {
    match &*state.read().unwrap() {
        FingerprintState::Pinned(set) => Some(set.clone()),
        _ => None,
    }
}

fn initial_state(pki: &Pki, genuine_already_pinned: bool) -> FingerprintState {
    if genuine_already_pinned {
        FingerprintState::Pinned(HashSet::from([pki.genuine.fingerprint.clone()]))
    } else {
        FingerprintState::Bootstrap
    }
}

/// Full error chain of a reqwest error (the TLS reason is in a source).
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        out.push_str(" | ");
        out.push_str(&inner.to_string());
        source = inner.source();
    }
    out
}

/// A backend presenting a valid certificate that is not the attested one is
/// refused, nothing is pinned, and no inference request reaches it. With
/// `genuine_already_pinned`, the relay's certificate first fails the pinned
/// fast-path probe, which falls through to the slow path.
async fn assert_relay_refused(transport: Transport, genuine_already_pinned: bool) {
    let pki = pki();
    let relay = start_backend("relay", &pki.relay, transport, &pki.genuine.fingerprint).await;
    let initial = initial_state(&pki, genuine_already_pinned);
    let state = Arc::new(std::sync::RwLock::new(initial.clone()));
    let metrics = Arc::new(CapturingMetricsService::new());
    let verifier = pool_verifier(&pki, state.clone(), metrics.clone());

    let err = channel_binding_error(verifier.create_verified_client(&relay.base_url).await);
    assert!(err.contains("TLS channel binding mismatch"), "{err}");
    assert!(err.contains(&pki.relay.fingerprint[..16]), "{err}");
    assert!(err.contains(&pki.genuine.fingerprint[..16]), "{err}");

    let c = &relay.counters;
    assert_eq!(count(&c.attestation_requests), 1);
    assert_eq!(count(&c.attestation_requests_with_auth), 0);
    assert_eq!(count(&c.completion_requests), 0);
    // Only the Bootstrap connection of the slow path completed a handshake
    // (a pinned fast-path probe is rejected during the handshake).
    assert_eq!(count(&c.connections), 1);
    // The shared pin set is unchanged.
    let expected = match initial {
        FingerprintState::Pinned(set) => Some(set),
        _ => None,
    };
    assert_eq!(pinned_set(&state), expected);
    assert_eq!(channel_binding_results(&metrics), vec!["result:mismatch"]);
}

/// The genuine backend is pinned, and the returned client serves inference on
/// the connection that carried the report: no second handshake.
async fn assert_genuine_accepted(transport: Transport) {
    let pki = pki();
    let genuine = start_backend("genuine", &pki.genuine, transport, &pki.genuine.fingerprint).await;
    let state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
    let metrics = Arc::new(CapturingMetricsService::new());
    let verifier = pool_verifier(&pki, state.clone(), metrics.clone());

    let client = verifier
        .create_verified_client(&genuine.base_url)
        .await
        .expect("the attested backend must be accepted");
    let c = &genuine.counters;
    assert_eq!(count(&c.attestation_requests), 1);
    assert_eq!(count(&c.attestation_requests_with_auth), 0);
    assert_eq!(count(&c.connections), 1);
    assert_eq!(
        pinned_set(&state),
        Some(HashSet::from([pki.genuine.fingerprint.clone()]))
    );
    assert_eq!(channel_binding_results(&metrics), vec!["result:match"]);

    let resp = client
        .post(format!("{}/v1/chat/completions", genuine.base_url))
        .json(&serde_json::json!({ "messages": [] }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert_eq!(
        inference_providers::spki_verifier::peer_spki_fingerprint(&resp).unwrap(),
        pki.genuine.fingerprint
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["served_by"], "genuine");
    assert_eq!(count(&c.completion_requests), 1);
    assert_eq!(
        count(&c.connections),
        1,
        "inference must reuse the connection that carried the attestation report"
    );
}

#[tokio::test]
async fn relay_certificate_is_refused_h2() {
    assert_relay_refused(Transport::TlsH2, false).await;
}

#[tokio::test]
async fn relay_certificate_is_refused_http1() {
    assert_relay_refused(Transport::TlsHttp1, false).await;
}

/// The genuine key is already pinned: the relay's certificate fails the
/// fast-path probe, and the slow path it falls through to must refuse it too
/// and leave the pin set as it was.
#[tokio::test]
async fn relay_certificate_is_refused_after_fast_path_rejection() {
    assert_relay_refused(Transport::TlsH2, true).await;
}

#[tokio::test]
async fn attested_backend_is_pinned_and_keeps_its_connection_h2() {
    assert_genuine_accepted(Transport::TlsH2).await;
}

#[tokio::test]
async fn attested_backend_is_pinned_and_keeps_its_connection_http1() {
    assert_genuine_accepted(Transport::TlsHttp1).await;
}

/// A backend that answers the attestation request with a redirect to the
/// attested backend is refused. Following the redirect would take the report
/// and its certificate from the redirect target's connection, while the
/// connection to the redirecting backend stayed in the client's pool.
#[tokio::test]
async fn attestation_redirect_is_not_followed() {
    let pki = pki();
    let genuine = start_backend(
        "genuine",
        &pki.genuine,
        Transport::TlsH2,
        &pki.genuine.fingerprint,
    )
    .await;
    let relay = start_backend_with(
        &pki.relay,
        Transport::TlsH2,
        Behavior {
            name: "relay",
            report: report(&pki.genuine.fingerprint, SIGNING_ADDRESS).to_string(),
            redirect_to: Some(genuine.base_url.clone()),
            close_connections: false,
        },
    )
    .await;
    let state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
    let metrics = Arc::new(CapturingMetricsService::new());
    let verifier = pool_verifier(&pki, state.clone(), metrics.clone());

    let err = other_error(verifier.create_verified_client(&relay.base_url).await);
    assert!(err.contains("307"), "{err}");
    assert_eq!(count(&relay.counters.attestation_requests), 1);
    assert_eq!(count(&genuine.counters.attestation_requests), 0);
    assert_eq!(count(&genuine.counters.connections), 0);
    assert_eq!(pinned_set(&state), None);
    assert!(channel_binding_results(&metrics).is_empty());
}

/// Reports may encode the fingerprint with a `0x` prefix or in upper case. The
/// pin must be stored in the canonical form the TLS verifier computes, or
/// every later handshake to the same backend would be rejected.
#[tokio::test]
async fn attested_fingerprint_is_pinned_in_canonical_form() {
    let pki = pki();
    let reported = format!("0x{}", pki.genuine.fingerprint.to_uppercase());
    let genuine = start_backend("genuine", &pki.genuine, Transport::TlsH2, &reported).await;
    let state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
    let metrics = Arc::new(CapturingMetricsService::new());
    let verifier = pool_verifier(&pki, state.clone(), metrics.clone());

    verifier
        .create_verified_client(&genuine.base_url)
        .await
        .expect("the attested backend must be accepted");
    assert_eq!(
        pinned_set(&state),
        Some(HashSet::from([pki.genuine.fingerprint.clone()]))
    );

    // With a pin in place, the next client takes the fast path: a new
    // connection whose handshake is checked against the pin set.
    verifier
        .create_verified_client(&genuine.base_url)
        .await
        .expect("the pinned fast path must accept the attested backend");
    assert_eq!(count(&genuine.counters.connections), 2);
    assert_eq!(count(&genuine.counters.attestation_requests), 1);
}

/// A verified report that attests no TLS fingerprint is refused, and counted
/// separately from a key mismatch.
#[tokio::test]
async fn report_without_fingerprint_is_refused() {
    let pki = pki();
    let genuine = start_backend(
        "genuine",
        &pki.genuine,
        Transport::TlsH2,
        &pki.genuine.fingerprint,
    )
    .await;
    let state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
    let metrics = Arc::new(CapturingMetricsService::new());
    let verifier = pool_verifier_with(
        &pki,
        state.clone(),
        metrics.clone(),
        Arc::new(NoFingerprintVerifier),
    );

    let err = channel_binding_error(verifier.create_verified_client(&genuine.base_url).await);
    assert!(err.contains("TLS channel binding unattested"), "{err}");
    assert_eq!(pinned_set(&state), None);
    assert_eq!(channel_binding_results(&metrics), vec!["result:unattested"]);
}

/// Without TLS there is no peer certificate to check: the backend is refused
/// (fail closed) rather than pinned on the report alone.
#[tokio::test]
async fn backend_without_peer_certificate_is_refused() {
    let pki = pki();
    let backend = start_backend(
        "plain",
        &pki.genuine,
        Transport::PlainHttp1,
        &pki.genuine.fingerprint,
    )
    .await;
    let state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
    let metrics = Arc::new(CapturingMetricsService::new());
    let verifier = pool_verifier(&pki, state.clone(), metrics.clone());

    let err = channel_binding_error(verifier.create_verified_client(&backend.base_url).await);
    assert!(err.contains("TLS channel binding missing"), "{err}");
    assert_eq!(count(&backend.counters.attestation_requests), 1);
    assert!(matches!(
        *state.read().unwrap(),
        FingerprintState::Bootstrap
    ));
    assert_eq!(channel_binding_results(&metrics), vec!["result:missing"]);
}

// ---------------------------------------------------------------------------
// Relays forwarding a genuinely bound report (production report_data check)
// ---------------------------------------------------------------------------

/// An HTTP/1.1-only relay forwarding the genuine, correctly bound report
/// passes the report_data check and is refused by the channel check, from
/// Bootstrap and after the fast path rejected it. The failure is typed as a
/// channel-binding failure, which the provider does not retry.
#[tokio::test]
async fn relay_with_quote_bound_report_is_refused() {
    for genuine_already_pinned in [false, true] {
        let pki = pki();
        let relay = start_backend(
            "relay",
            &pki.relay,
            Transport::TlsHttp1,
            &pki.genuine.fingerprint,
        )
        .await;
        let initial = initial_state(&pki, genuine_already_pinned);
        let state = Arc::new(std::sync::RwLock::new(initial.clone()));
        let metrics = Arc::new(CapturingMetricsService::new());
        let verifier = pool_verifier_with(
            &pki,
            state.clone(),
            metrics.clone(),
            Arc::new(QuoteBoundVerifier {
                genuine_fingerprint: pki.genuine.fingerprint.clone(),
            }),
        );

        let err = channel_binding_error(verifier.create_verified_client(&relay.base_url).await);
        assert!(err.contains("TLS channel binding mismatch"), "{err}");
        let c = &relay.counters;
        assert_eq!(count(&c.attestation_requests), 1);
        assert_eq!(count(&c.attestation_requests_with_auth), 0);
        assert_eq!(count(&c.completion_requests), 0);
        let expected =
            genuine_already_pinned.then(|| HashSet::from([pki.genuine.fingerprint.clone()]));
        assert_eq!(pinned_set(&state), expected);
        assert_eq!(channel_binding_results(&metrics), vec!["result:mismatch"]);
    }
}

/// An L4 splice passes the first connection through to the genuine backend,
/// so the attestation and its channel check genuinely pass. The genuine
/// backend closes that connection, and the splice terminates every later
/// connection with the relay's own valid certificate. The returned client must
/// refuse the relay on reconnect (its own pin is the attested key, and TLS
/// session resumption is off), so no inference reaches the relay.
#[tokio::test]
async fn splicing_relay_cannot_take_over_a_verified_client() {
    let pki = pki();
    let genuine = start_backend_with(
        &pki.genuine,
        Transport::TlsHttp1,
        Behavior {
            name: "genuine",
            report: report(&pki.genuine.fingerprint, SIGNING_ADDRESS).to_string(),
            redirect_to: None,
            close_connections: true,
        },
    )
    .await;
    let genuine_addr: std::net::SocketAddr = genuine
        .base_url
        .trim_start_matches("https://")
        .parse()
        .unwrap();

    let relay_acceptor = tls_acceptor(&pki.relay, Transport::TlsHttp1).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let splice_url = format!("https://{}", listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let relay_handshakes = Arc::new(AtomicUsize::new(0));
    let relay_completions = Arc::new(AtomicUsize::new(0));
    {
        let accepted = accepted.clone();
        let relay_handshakes = relay_handshakes.clone();
        let relay_completions = relay_completions.clone();
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                let n = accepted.fetch_add(1, Ordering::SeqCst);
                let acceptor = relay_acceptor.clone();
                let relay_handshakes = relay_handshakes.clone();
                let relay_completions = relay_completions.clone();
                tokio::spawn(async move {
                    if n == 0 {
                        // L4 passthrough to the genuine backend.
                        let mut upstream =
                            tokio::net::TcpStream::connect(genuine_addr).await.unwrap();
                        let _ = tokio::io::copy_bidirectional(&mut tcp, &mut upstream).await;
                        return;
                    }
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    relay_handshakes.fetch_add(1, Ordering::SeqCst);
                    let service = service_fn(move |req: Request<Incoming>| {
                        let relay_completions = relay_completions.clone();
                        async move {
                            if req.uri().path() == "/v1/chat/completions" {
                                relay_completions.fetch_add(1, Ordering::SeqCst);
                            }
                            let _ = req.into_body().collect().await;
                            Ok::<_, std::convert::Infallible>(Response::new(Full::new(
                                Bytes::from_static(br#"{"served_by":"relay"}"#),
                            )))
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                });
            }
        });
    }

    let state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
    let metrics = Arc::new(CapturingMetricsService::new());
    let verifier = pool_verifier_with(
        &pki,
        state.clone(),
        metrics.clone(),
        Arc::new(QuoteBoundVerifier {
            genuine_fingerprint: pki.genuine.fingerprint.clone(),
        }),
    );
    let client = verifier
        .create_verified_client(&splice_url)
        .await
        .expect("the attestation passed through to the genuine backend must be accepted");
    assert_eq!(channel_binding_results(&metrics), vec!["result:match"]);
    assert_eq!(count(&genuine.counters.attestation_requests), 1);

    // The genuine backend closed the attestation connection, so inference
    // opens a new one, which the splice hands to the relay.
    for _ in 0..2 {
        let err = client
            .post(format!("{splice_url}/v1/chat/completions"))
            .json(&serde_json::json!({ "messages": [] }))
            .send()
            .await
            .expect_err("the relay's certificate must be refused on reconnect");
        let chain = error_chain(&err);
        assert!(
            chain.contains("does not match any attested fingerprint"),
            "unexpected error: {chain}"
        );
    }
    assert_eq!(relay_completions.load(Ordering::SeqCst), 0);
    assert_eq!(relay_handshakes.load(Ordering::SeqCst), 0);
    assert_eq!(accepted.load(Ordering::SeqCst), 3);
    assert_eq!(count(&genuine.counters.completion_requests), 0);
    assert_eq!(count(&genuine.counters.connections), 1);
}

/// A relay that rewrites the report to attest its own key fails the
/// report_data check (the quote binds the genuine key) before the channel
/// check: nothing is pinned and no channel-binding result is recorded.
#[tokio::test]
async fn report_attesting_the_relay_key_fails_verification() {
    for genuine_already_pinned in [false, true] {
        let pki = pki();
        let relay = start_backend(
            "relay",
            &pki.relay,
            Transport::TlsH2,
            &pki.relay.fingerprint,
        )
        .await;
        let state = Arc::new(std::sync::RwLock::new(initial_state(
            &pki,
            genuine_already_pinned,
        )));
        let metrics = Arc::new(CapturingMetricsService::new());
        let verifier = pool_verifier_with(
            &pki,
            state.clone(),
            metrics.clone(),
            Arc::new(QuoteBoundVerifier {
                genuine_fingerprint: pki.genuine.fingerprint.clone(),
            }),
        );

        let err = other_error(verifier.create_verified_client(&relay.base_url).await);
        assert!(err.contains("Attestation verification failed"), "{err}");
        assert!(err.contains("report data binding mismatch"), "{err}");
        assert!(channel_binding_results(&metrics).is_empty());
        assert_eq!(count(&relay.counters.completion_requests), 0);
        let expected =
            genuine_already_pinned.then(|| HashSet::from([pki.genuine.fingerprint.clone()]));
        assert_eq!(pinned_set(&state), expected);
    }
}

/// The connection is compared with the verifier's output, not with the raw
/// report: a report claiming the key the connection presents is refused when
/// the verified fingerprint differs, and a report with a wrong field is
/// accepted (and the verified value pinned) when the verified fingerprint
/// matches the connection.
#[tokio::test]
async fn channel_check_uses_the_verified_fingerprint() {
    let pki = pki();
    let state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
    let metrics = Arc::new(CapturingMetricsService::new());
    let verifier = pool_verifier_with(
        &pki,
        state.clone(),
        metrics.clone(),
        Arc::new(FixedFingerprintVerifier {
            fingerprint: pki.genuine.fingerprint.clone(),
        }),
    );

    let relay = start_backend(
        "relay",
        &pki.relay,
        Transport::TlsH2,
        &pki.relay.fingerprint,
    )
    .await;
    let err = channel_binding_error(verifier.create_verified_client(&relay.base_url).await);
    assert!(err.contains("TLS channel binding mismatch"), "{err}");
    assert_eq!(pinned_set(&state), None);

    let genuine = start_backend(
        "genuine",
        &pki.genuine,
        Transport::TlsH2,
        &pki.relay.fingerprint,
    )
    .await;
    verifier
        .create_verified_client(&genuine.base_url)
        .await
        .expect("the connection presents the verified fingerprint");
    assert_eq!(
        pinned_set(&state),
        Some(HashSet::from([pki.genuine.fingerprint.clone()]))
    );
}

/// report_data binds `SHA256(signing_address || fingerprint)` without length
/// framing, so bytes can be moved from the fingerprint into the address and
/// the report_data check still passes. The resulting "verified" fingerprint is
/// truncated and cannot equal the SPKI hash of any connection.
#[tokio::test]
async fn shifted_report_data_bytes_fail_the_channel_check() {
    let pki = pki();
    let genuine = &pki.genuine.fingerprint;
    let shifted_address = format!("{SIGNING_ADDRESS}{}", &genuine[..16]);
    let shifted_fingerprint = &genuine[16..];
    let relay = start_backend_with(
        &pki.relay,
        Transport::TlsH2,
        Behavior {
            name: "relay",
            report: report(shifted_fingerprint, &shifted_address).to_string(),
            redirect_to: None,
            close_connections: false,
        },
    )
    .await;
    let state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
    let metrics = Arc::new(CapturingMetricsService::new());
    let verifier = pool_verifier_with(
        &pki,
        state.clone(),
        metrics.clone(),
        Arc::new(QuoteBoundVerifier {
            genuine_fingerprint: genuine.clone(),
        }),
    );

    // The report_data check passed: the failure is the channel check.
    let err = channel_binding_error(verifier.create_verified_client(&relay.base_url).await);
    assert!(err.contains("TLS channel binding mismatch"), "{err}");
    assert_eq!(pinned_set(&state), None);
    assert_eq!(channel_binding_results(&metrics), vec!["result:mismatch"]);
}

#[test]
fn channel_binding_check_classifies_outcomes() {
    let fp = "ab".repeat(32);
    let observed: Result<String, String> = Ok(fp.clone());
    assert_eq!(
        ChannelBinding::check(&observed, Some(&fp)),
        ChannelBinding::Match
    );
    assert_eq!(
        ChannelBinding::check(&observed, Some(&fp.to_uppercase())),
        ChannelBinding::Match
    );
    assert_eq!(
        ChannelBinding::check(&observed, Some(&format!("0x{fp}"))),
        ChannelBinding::Match
    );
    assert_eq!(
        ChannelBinding::check(&observed, Some(&"cd".repeat(32))),
        ChannelBinding::Mismatch
    );
    assert_eq!(
        ChannelBinding::check(&observed, None),
        ChannelBinding::Unattested
    );
    assert_eq!(
        ChannelBinding::check(&Err("no TLS".to_string()), Some(&fp)),
        ChannelBinding::Missing
    );
}

/// The signing key and TEE identity of a discovery probe are only used when
/// its channel check passed; the fingerprint is pinned either way.
#[test]
fn discovery_uses_signing_keys_only_from_matching_probes() {
    let report = serde_json::json!({
        "signing_public_key": "04abcd",
        "info": { "app_id": "app", "key_provider_info": { "id": "root" } },
    });
    let report = report.as_object().unwrap();

    let probe = backend_probe(ChannelBinding::Match, report, 2, "ecdsa").unwrap();
    assert_eq!(probe.index, 2);
    assert_eq!(probe.algo, "ecdsa");
    assert_eq!(probe.pubkey, "04abcd");
    assert_eq!(
        probe.identity,
        Some(("root".to_string(), "app".to_string()))
    );

    for binding in [
        ChannelBinding::Mismatch,
        ChannelBinding::Unattested,
        ChannelBinding::Missing,
    ] {
        assert!(backend_probe(binding, report, 2, "ecdsa").is_none());
    }

    let no_key = serde_json::Map::new();
    assert!(backend_probe(ChannelBinding::Match, &no_key, 2, "ecdsa").is_none());
}
