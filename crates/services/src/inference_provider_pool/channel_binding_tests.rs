//! Channel binding on the inline verification path
//! (`PoolBackendVerifier::create_verified_client`, slow path).
//!
//! Two local TLS backends present certificates for 127.0.0.1 from the same
//! test CA: "genuine" and "relay". Both serve an attestation report whose TLS
//! fingerprint is the genuine backend's, as a relay forwarding the genuine
//! report would. The attestation verifier is a stub that accepts the report
//! and returns the fingerprint it carries, so these tests exercise the
//! connection handling only. Expected: the relay is refused and nothing is
//! pinned; the genuine backend is pinned and the returned client keeps serving
//! on the connection that carried the report.

use super::*;
use crate::attestation::{AttestationVerificationError, VerifiedAttestation};
use crate::metrics::capturing::CapturingMetricsService;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use inference_providers::spki_verifier::compute_spki_fingerprint_from_der;
use inference_providers::BackendVerifier as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::TcpListener;

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
        Ok(VerifiedAttestation {
            tls_cert_fingerprint: attestation_report
                .get("tls_cert_fingerprint")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            signing_address: "0x0000000000000000000000000000000000000001".to_string(),
            tcb_status: "UpToDate".to_string(),
            advisory_ids: Vec::new(),
            os_image_hash: None,
            compose_hash: None,
            gpu_verdict: None,
        })
    }
}

/// Accepts any report but attests no TLS fingerprint.
struct NoFingerprintVerifier;

#[async_trait::async_trait]
impl BackendAttestationVerifier for NoFingerprintVerifier {
    async fn verify_attestation_report(
        &self,
        attestation_report: &serde_json::Map<String, serde_json::Value>,
        request_nonce: &str,
    ) -> Result<VerifiedAttestation, AttestationVerificationError> {
        let mut verified = AcceptReportVerifier
            .verify_attestation_report(attestation_report, request_nonce)
            .await?;
        verified.tls_cert_fingerprint = None;
        Ok(verified)
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

async fn handle(
    name: &'static str,
    report: Arc<String>,
    redirect_to: Option<Arc<String>>,
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
    let (status, body) = match (method, path.as_str()) {
        (Method::GET, "/v1/attestation/report") => {
            counters.attestation_requests.fetch_add(1, Ordering::SeqCst);
            if has_auth {
                counters
                    .attestation_requests_with_auth
                    .fetch_add(1, Ordering::SeqCst);
            }
            if let Some(target) = redirect_to {
                return Ok(Response::builder()
                    .status(StatusCode::TEMPORARY_REDIRECT)
                    .header("location", format!("{target}{path_and_query}"))
                    .body(Full::new(Bytes::new()))
                    .unwrap());
            }
            (StatusCode::OK, report.as_str().to_string())
        }
        (Method::GET, "/v1/models") => {
            (StatusCode::OK, r#"{"object":"list","data":[]}"#.to_string())
        }
        (Method::POST, "/v1/chat/completions") => {
            counters.completion_requests.fetch_add(1, Ordering::SeqCst);
            (
                StatusCode::OK,
                serde_json::json!({ "served_by": name }).to_string(),
            )
        }
        _ => (StatusCode::NOT_FOUND, "{}".to_string()),
    };
    Ok(Response::builder()
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
    start_backend_with(name, leaf, transport, attested_fingerprint, None).await
}

/// Like [`start_backend`]; with `redirect_to`, the attestation request is
/// answered with a 307 to the same path and query under `redirect_to`.
async fn start_backend_with(
    name: &'static str,
    leaf: &Leaf,
    transport: Transport,
    attested_fingerprint: &str,
    redirect_to: Option<String>,
) -> Backend {
    let acceptor = match transport {
        Transport::PlainHttp1 => None,
        Transport::TlsH2 | Transport::TlsHttp1 => {
            let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![leaf.cert.clone()], leaf.key.clone_key())
            .unwrap();
            config.alpn_protocols = if transport == Transport::TlsH2 {
                vec![b"h2".to_vec(), b"http/1.1".to_vec()]
            } else {
                vec![b"http/1.1".to_vec()]
            };
            Some(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
        }
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = match acceptor {
        Some(_) => format!("https://{addr}"),
        None => format!("http://{addr}"),
    };
    let counters = Counters::default();
    let report = Arc::new(
        serde_json::json!({
            "tls_cert_fingerprint": attested_fingerprint,
            "signing_address": "0x0000000000000000000000000000000000000001",
            "intel_quote": "00",
        })
        .to_string(),
    );

    let redirect_to = redirect_to.map(Arc::new);
    let server_counters = counters.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let counters = server_counters.clone();
            let report = report.clone();
            let redirect_to = redirect_to.clone();
            tokio::spawn(async move {
                let connections = counters.connections.clone();
                let service = service_fn(move |req| {
                    handle(
                        name,
                        report.clone(),
                        redirect_to.clone(),
                        counters.clone(),
                        req,
                    )
                });
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

/// A backend presenting a valid certificate that is not the attested one is
/// refused, nothing is pinned, and no inference request reaches it. With
/// `genuine_already_pinned`, the relay's certificate first fails the pinned
/// fast-path probe, which falls through to the slow path.
async fn assert_relay_refused(transport: Transport, genuine_already_pinned: bool) {
    let pki = pki();
    let relay = start_backend("relay", &pki.relay, transport, &pki.genuine.fingerprint).await;
    let initial = if genuine_already_pinned {
        FingerprintState::Pinned(HashSet::from([pki.genuine.fingerprint.clone()]))
    } else {
        FingerprintState::Bootstrap
    };
    let state = Arc::new(std::sync::RwLock::new(initial.clone()));
    let metrics = Arc::new(CapturingMetricsService::new());
    let verifier = pool_verifier(&pki, state.clone(), metrics.clone());

    let err = verifier
        .create_verified_client(&relay.base_url)
        .await
        .expect_err("a backend whose certificate is not the attested one must be refused");
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
        "relay",
        &pki.relay,
        Transport::TlsH2,
        &pki.genuine.fingerprint,
        Some(genuine.base_url.clone()),
    )
    .await;
    let state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
    let metrics = Arc::new(CapturingMetricsService::new());
    let verifier = pool_verifier(&pki, state.clone(), metrics.clone());

    let err = verifier
        .create_verified_client(&relay.base_url)
        .await
        .expect_err("a redirected attestation fetch must be refused");
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
    let verifier = PoolBackendVerifier {
        attestation_verifier: Arc::new(NoFingerprintVerifier),
        ..pool_verifier(&pki, state.clone(), metrics.clone())
    };

    let err = verifier
        .create_verified_client(&genuine.base_url)
        .await
        .expect_err("a report without a TLS fingerprint must be refused");
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

    let err = verifier
        .create_verified_client(&backend.base_url)
        .await
        .expect_err("no peer certificate means no channel binding");
    assert!(err.contains("TLS channel binding missing"), "{err}");
    assert_eq!(count(&backend.counters.attestation_requests), 1);
    assert!(matches!(
        *state.read().unwrap(),
        FingerprintState::Bootstrap
    ));
    assert_eq!(channel_binding_results(&metrics), vec!["result:missing"]);
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
