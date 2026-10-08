//! Tests for the Tinfoil router session and provider. The TLS server here is a
//! local stand-in for `inference.tinfoil.sh`; nothing talks to the live API.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::StreamExt;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::availability::{map_upstream_status, UpstreamDisposition};
use super::session::{TinfoilRouterSession, VerifiedState};
use super::verifier_port::*;
use super::wire;
use super::{Config, Provider};
use crate::spki_verifier::{compute_spki_fingerprint_from_der, FingerprintState, SharedTlsRoots};
use crate::{ChatCompletionParams, CompletionError, InferenceProvider};

const SLUG: &str = "gpt-oss-120b";
const CANON: &str = "openai/gpt-oss-120b";

fn fixture(name: &str) -> Vec<u8> {
    let p = format!(
        "{}/src/attested/tinfoil/testdata/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(p).unwrap()
}

// ---------------------------------------------------------------- availability

#[test]
fn status_mapping_matches_spec() {
    assert_eq!(map_upstream_status(503), UpstreamDisposition::Retryable503);
    assert_eq!(map_upstream_status(500), UpstreamDisposition::Retryable503);
    assert_eq!(
        map_upstream_status(429),
        UpstreamDisposition::Passthrough429
    );
    for s in [401, 402, 403] {
        assert_eq!(map_upstream_status(s), UpstreamDisposition::Retryable503);
    }
    assert_eq!(
        map_upstream_status(400),
        UpstreamDisposition::ReturnAs4xx(400)
    );
    assert_eq!(
        map_upstream_status(422),
        UpstreamDisposition::ReturnAs4xx(422)
    );
    // A redirect is never followed or trusted: unavailable.
    assert_eq!(map_upstream_status(302), UpstreamDisposition::Retryable503);
}

#[test]
fn unavailable_is_external_503() {
    match super::availability::unavailable("not_verified") {
        CompletionError::HttpError {
            status_code,
            is_external,
            message,
        } => {
            assert_eq!(status_code, 503);
            assert!(is_external);
            assert!(message.contains("not_verified"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn api_key_never_in_debug() {
    let c = Config::new("tk_secret_value".into(), 30);
    assert!(!format!("{c:?}").contains("tk_secret_value"));
}

#[test]
fn production_urls_are_constants() {
    let c = Config::new("k".into(), 30);
    assert_eq!(c.base_url, super::BASE_URL);
    assert_eq!(c.atc_url, super::ATC_URL);
    assert_eq!(super::BASE_URL, "https://inference.tinfoil.sh");
    assert_eq!(super::ATC_URL, "https://atc.tinfoil.sh/attestation");
    assert_eq!(super::PROXY_REREAD.as_secs(), 60);
    assert_eq!(super::ROUTER_REVERIFY.as_secs(), 300);
}

// ---------------------------------------------------------------- wire mapping

#[test]
fn nonstream_fixture_maps_model_and_reasoning() {
    let (bytes, resp) = wire::map_response(&fixture("chat_nonstream.json"), CANON).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["model"], CANON);
    assert_eq!(resp.model, CANON);
    let msg = &v["choices"][0]["message"];
    assert!(msg["reasoning_content"]
        .as_str()
        .unwrap()
        .starts_with("The user says"));
    assert!(msg.get("reasoning").is_none());
    assert_eq!(v["choices"][0]["finish_reason"], "length");
    let s = String::from_utf8(bytes).unwrap();
    for k in [
        "token_ids",
        "prompt_token_ids",
        "prompt_text",
        "stop_reason",
        "routed_experts",
        "kv_transfer_params",
        "prompt_logprobs",
    ] {
        assert!(!s.contains(k), "{k} leaked");
    }
    assert_eq!(resp.usage.prompt_tokens, 69);
}

#[test]
fn nonstream_glm_fixture_has_no_reasoning_and_keeps_usage_details() {
    let (bytes, resp) =
        wire::map_response(&fixture("chat_nonstream_glm53_high.json"), "zai/glm-5.3").unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v["choices"][0]["message"]
        .get("reasoning_content")
        .is_none());
    assert_eq!(resp.usage.reasoning_tokens(), Some(0));
}

fn parse_sse(fixture_name: &str) -> crate::StreamingResult {
    let bytes = bytes::Bytes::from(fixture(fixture_name));
    let s = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes)]);
    Box::pin(crate::sse_parser::new_external_sse_parser(s, true))
}

async fn run_stream(name: &str, include_usage: bool) -> Vec<crate::SSEEvent> {
    let mapped = wire::map_stream(parse_sse(name), CANON.to_string(), include_usage);
    mapped.map(|e| e.unwrap()).collect().await
}

fn client_json(ev: &crate::SSEEvent) -> Option<serde_json::Value> {
    let s = std::str::from_utf8(&ev.raw_bytes).ok()?;
    let d = s.trim().strip_prefix("data:")?.trim();
    serde_json::from_str(d).ok()
}

#[tokio::test]
async fn stream_fixture_maps_deltas_and_usage() {
    let evs = run_stream("chat_stream.sse", true).await;
    let mut reasoning = String::new();
    let mut usage_chunks = 0;
    for ev in &evs {
        let raw = String::from_utf8_lossy(&ev.raw_bytes).to_string();
        for k in [
            "\"p\"",
            "token_ids",
            "prompt_text",
            "stop_reason",
            "\"reasoning\"",
        ] {
            assert!(!raw.contains(k), "{k} leaked: {raw}");
        }
        if let Some(v) = client_json(ev) {
            assert_eq!(v["model"], CANON);
            if let Some(r) = v["choices"][0]["delta"]["reasoning_content"].as_str() {
                reasoning.push_str(r);
            }
            if v.get("usage").is_some() {
                usage_chunks += 1;
                assert!(
                    v["choices"].as_array().unwrap().is_empty(),
                    "usage only on final chunk"
                );
                assert_eq!(v["usage"]["completion_tokens"], 16);
            }
        }
    }
    assert!(reasoning.starts_with("The user says"));
    assert_eq!(usage_chunks, 1);
    assert!(evs.iter().any(|e| e.is_done_marker()));
}

#[tokio::test]
async fn stream_without_client_usage_hides_usage_but_keeps_it_for_billing() {
    let evs = run_stream("chat_stream.sse", false).await;
    let mut billed = None;
    for ev in &evs {
        assert!(!String::from_utf8_lossy(&ev.raw_bytes).contains("usage"));
        if let Some(crate::StreamChunk::Chat(c)) = &ev.chunk {
            if let Some(u) = &c.usage {
                billed = Some(u.completion_tokens);
            }
        }
    }
    assert_eq!(billed, Some(16));
}

#[tokio::test]
async fn stream_no_usage_fixture_strips_cumulative_per_chunk_usage() {
    let evs = run_stream("chat_stream_no_usage.sse", false).await;
    for ev in &evs {
        assert!(!String::from_utf8_lossy(&ev.raw_bytes).contains("usage"));
    }
    // Billing still sees the last cumulative usage.
    let last = evs
        .iter()
        .filter_map(|e| match &e.chunk {
            Some(crate::StreamChunk::Chat(c)) => c.usage.clone(),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(last.completion_tokens, 16);
}

// ---------------------------------------------------------------- TLS server

struct TestPki {
    roots: SharedTlsRoots,
    leaf_a: CertificateDer<'static>,
    key_a: PrivateKeyDer<'static>,
    leaf_b: CertificateDer<'static>,
    key_b: PrivateKeyDer<'static>,
}

fn test_pki() -> TestPki {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca_params, ca_key);
    let mk = || {
        let k = rcgen::KeyPair::generate().unwrap();
        let p = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        let c = p.signed_by(&k, &issuer).unwrap();
        (
            c.der().clone(),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(k.serialize_der())),
        )
    };
    let (leaf_a, key_a) = mk();
    let (leaf_b, key_b) = mk();
    let mut store = rustls::RootCertStore::empty();
    store.add(ca_cert.der().clone()).unwrap();
    TestPki {
        roots: SharedTlsRoots::from_root_store(store),
        leaf_a,
        key_a,
        leaf_b,
        key_b,
    }
}

fn acceptor(
    cert: &CertificateDer<'static>,
    key: &PrivateKeyDer<'static>,
) -> tokio_rustls::TlsAcceptor {
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key.clone_key())
    .unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

#[derive(Clone)]
struct ChatReply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

type LastChat = Arc<Mutex<Option<(String, Vec<u8>)>>>;

struct TestServer {
    addr: SocketAddr,
    acceptor: Arc<Mutex<tokio_rustls::TlsAcceptor>>,
    chat: Arc<Mutex<ChatReply>>,
    last_chat: LastChat,
}

async fn start_server(pki: &TestPki) -> TestServer {
    let acc = Arc::new(Mutex::new(acceptor(&pki.leaf_a, &pki.key_a)));
    let chat = Arc::new(Mutex::new(ChatReply {
        status: 200,
        content_type: "application/json",
        body: fixture("chat_nonstream.json"),
    }));
    let last_chat = Arc::new(Mutex::new(None));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (acc2, chat2, last2) = (acc.clone(), chat.clone(), last_chat.clone());
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let a = acc2.lock().unwrap().clone();
            let (chat, last) = (chat2.clone(), last2.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = a.accept(tcp).await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    match tls.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let len = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    match tls.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let body = buf[head_end..head_end + len].to_vec();
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let (status, ct, out): (u16, &str, Vec<u8>) = match path.as_str() {
                    "/attestation" => (200, "application/json", atc_json()),
                    "/.well-known/tinfoil-proxy" => (200, "application/json", proxy_json()),
                    "/v1/models" => (200, "application/json", fixture("models.json")),
                    "/v1/chat/completions" => {
                        *last.lock().unwrap() = Some((head.clone(), body));
                        let r = chat.lock().unwrap().clone();
                        (r.status, r.content_type, r.body)
                    }
                    _ => (404, "text/plain", b"nf".to_vec()),
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    out.len()
                );
                let _ = tls.write_all(resp.as_bytes()).await;
                let _ = tls.write_all(&out).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    TestServer {
        addr,
        acceptor: acc,
        chat,
        last_chat,
    }
}

fn atc_json() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "domain": "inference.tinfoil.sh",
        "enclaveAttestationReport": {"format": "sev-snp-guest/v2", "body": "cmVwb3J0"},
        "vcek": "dmNlaw==",
        "enclaveCert": "Y2VydA==",
        "digest": "d",
        "sigstoreBundle": {}
    }))
    .unwrap()
}

fn proxy_doc() -> serde_json::Value {
    serde_json::json!({"models": {
        SLUG: {
            "repo": "tinfoilsh/confidential-gpt-oss-120b",
            "tag": "v0.0.9",
            "measurement": {"type": "snp-tdx-multiplatform/v1", "registers": ["aa", "bb"]},
            "enclaves": {"h1.example": {"tls_key_fp": "fp1", "hpke_key": "hk", "predicate": "pr"}}
        },
        "other-model": {
            "repo": "r", "tag": "t",
            "measurement": {"type": "x", "registers": ["cc"]},
            "enclaves": {}
        }
    }})
}

fn proxy_json() -> Vec<u8> {
    serde_json::to_vec(&proxy_doc()).unwrap()
}

struct StubVerifier {
    spki: Mutex<[u8; 32]>,
    router_calls: AtomicUsize,
    deny_models: Mutex<Vec<String>>,
}

impl StubVerifier {
    fn new(spki_hex: &str) -> Arc<Self> {
        let mut a = [0u8; 32];
        a.copy_from_slice(&hex::decode(spki_hex).unwrap());
        Arc::new(Self {
            spki: Mutex::new(a),
            router_calls: AtomicUsize::new(0),
            deny_models: Mutex::new(vec![]),
        })
    }
}

impl TinfoilVerifier for StubVerifier {
    fn verify_router(&self, _b: &AtcBundle) -> Result<VerifiedRouter, TinfoilVerifyError> {
        self.router_calls.fetch_add(1, Ordering::SeqCst);
        Ok(VerifiedRouter {
            spki_sha256: *self.spki.lock().unwrap(),
            measurement_hex: "ab".repeat(48),
            tag: "tinfoilsh/confidential-model-router@v0.0.155".into(),
        })
    }
    fn check_model(
        &self,
        slug: &str,
        entry: &ProxyModelEntry,
    ) -> Result<PinnedModel, TinfoilVerifyError> {
        if self.deny_models.lock().unwrap().iter().any(|s| s == slug) {
            return Err(TinfoilVerifyError::UnknownModelMeasurement);
        }
        Ok(PinnedModel {
            slug: slug.into(),
            repo: entry.repo.clone(),
            tag: entry.tag.clone(),
            entry: entry.clone(),
        })
    }
}

struct Env {
    session: Arc<TinfoilRouterSession>,
    verifier: Arc<StubVerifier>,
    server: TestServer,
    provider: Provider,
    pki: TestPki,
}

async fn env() -> Env {
    let pki = test_pki();
    let server = start_server(&pki).await;
    let cfg = Config::new("tk_test_key".into(), 5).with_urls(
        &format!("https://{}", server.addr),
        &format!("https://{}/attestation", server.addr),
    );
    let verifier =
        StubVerifier::new(&compute_spki_fingerprint_from_der(pki.leaf_a.as_ref()).unwrap());
    let session =
        TinfoilRouterSession::new_with_roots(cfg.clone(), verifier.clone(), pki.roots.clone())
            .unwrap();
    let provider = Provider::new(session.clone(), &cfg, SLUG.into(), CANON.into());
    Env {
        session,
        verifier,
        server,
        provider,
        pki,
    }
}

fn params(stream: bool, include_usage: Option<bool>) -> ChatCompletionParams {
    let mut v = serde_json::json!({
        "model": CANON,
        "messages": [{"role": "user", "content": "Say OK"}],
        "stream": stream,
        "x_org_id": "internal"
    });
    if let Some(u) = include_usage {
        v["stream_options"] = serde_json::json!({"include_usage": u});
    }
    serde_json::from_value(v).unwrap()
}

fn expect_http<T>(r: Result<T, CompletionError>, status: u16) -> String {
    match r.map(|_| ()) {
        Err(CompletionError::HttpError {
            status_code,
            is_external,
            message,
        }) => {
            assert_eq!(status_code, status, "{message}");
            assert!(is_external);
            message
        }
        other => panic!("expected HttpError {status}, got {other:?}"),
    }
}

#[tokio::test]
async fn session_starts_blocked_and_unverified_calls_are_503() {
    let e = env().await;
    assert!(matches!(
        e.session.fingerprint_state(),
        FingerprintState::Blocked
    ));
    let msg = expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        503,
    );
    assert!(msg.contains("not_verified"));
    let msg = expect_http(
        e.provider
            .chat_completion_stream(params(true, None), "h".into())
            .await,
        503,
    );
    assert!(msg.contains("not_verified"));
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 0);
    assert!(e.server.last_chat.lock().unwrap().is_none());
}

#[tokio::test]
async fn becomes_pinned_only_after_verify_then_serves_nonstream() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    match e.session.fingerprint_state() {
        FingerprintState::Pinned(s) => assert_eq!(s.len(), 1),
        other => panic!("{other:?}"),
    }
    assert!(e.session.model_status(SLUG).is_ok());
    assert!(e.session.model_status("nope").is_err());

    let r = e
        .provider
        .chat_completion(params(false, None), "h".into())
        .await
        .unwrap();
    assert_eq!(r.response.model, CANON);
    assert_eq!(r.serving.source, crate::ProviderSource::Tinfoil);
    assert_eq!(r.serving.tier, crate::ProviderTier::Attested3p);
    let v: serde_json::Value = serde_json::from_slice(&r.raw_bytes).unwrap();
    assert!(v["choices"][0]["message"]["reasoning_content"].is_string());

    let (head, body) = e.server.last_chat.lock().unwrap().clone().unwrap();
    assert!(head
        .to_ascii_lowercase()
        .contains("authorization: bearer tk_test_key"));
    let b: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(b["model"], SLUG);
    assert_eq!(b["stream"], false);
    assert!(
        b.get("x_org_id").is_none(),
        "internal keys never reach Tinfoil"
    );
}

#[tokio::test]
async fn stream_request_always_asks_upstream_for_usage_and_gates_for_client() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    *e.server.chat.lock().unwrap() = ChatReply {
        status: 200,
        content_type: "text/event-stream",
        body: fixture("chat_stream.sse"),
    };
    // Client did not ask for usage.
    let s = e
        .provider
        .chat_completion_stream(params(true, None), "h".into())
        .await
        .unwrap();
    let evs: Vec<_> = s.map(|x| x.unwrap()).collect().await;
    assert!(evs
        .iter()
        .all(|ev| !String::from_utf8_lossy(&ev.raw_bytes).contains("usage")));
    assert!(evs
        .iter()
        .any(|ev| matches!(&ev.chunk, Some(crate::StreamChunk::Chat(c)) if c.usage.is_some())));
    let (_, body) = e.server.last_chat.lock().unwrap().clone().unwrap();
    let b: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(b["stream_options"]["include_usage"], true);
    assert_eq!(b["model"], SLUG);

    // Client asked for usage.
    let s = e
        .provider
        .chat_completion_stream(params(true, Some(true)), "h".into())
        .await
        .unwrap();
    let evs: Vec<_> = s.map(|x| x.unwrap()).collect().await;
    assert!(evs
        .iter()
        .any(|ev| String::from_utf8_lossy(&ev.raw_bytes).contains("\"usage\"")));
}

#[tokio::test]
async fn upstream_statuses_map_per_spec() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    let set = |status: u16| {
        *e.server.chat.lock().unwrap() = ChatReply {
            status,
            content_type: "application/json",
            body: br#"{"error":{"message":"nope"}}"#.to_vec(),
        };
    };
    for s in [500u16, 502, 503] {
        set(s);
        expect_http(
            e.provider
                .chat_completion(params(false, None), "h".into())
                .await,
            503,
        );
    }
    assert_eq!(e.session.upstream_auth_failures(), 0);
    for (n, s) in [401u16, 402, 403].into_iter().enumerate() {
        set(s);
        let msg = expect_http(
            e.provider
                .chat_completion(params(false, None), "h".into())
                .await,
            503,
        );
        assert!(!msg.contains("nope"), "upstream auth error must not leak");
        assert_eq!(e.session.upstream_auth_failures(), n as u64 + 1);
    }
    set(429);
    expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        429,
    );
    set(400);
    expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        400,
    );
    set(422);
    expect_http(
        e.provider
            .chat_completion_stream(params(true, None), "h".into())
            .await,
        422,
    );
    assert_eq!(e.session.upstream_auth_failures(), 3);
}

#[tokio::test]
async fn spki_mismatch_reverifies_once_then_503() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 1);
    // The server rotates to a certificate (B) the attested SPKI (A) does not cover.
    *e.server.acceptor.lock().unwrap() = acceptor(&e.pki.leaf_b, &e.pki.key_b);

    expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        503,
    );
    assert_eq!(
        e.verifier.router_calls.load(Ordering::SeqCst),
        1 + super::session::ATC_ATTEMPTS,
        "exactly one re-verify (each tries a bounded number of fresh bundles)"
    );
    assert!(
        e.server.last_chat.lock().unwrap().is_none(),
        "no request ever reached the unpinned peer"
    );
    // Fail closed: verification could not complete, so later calls stay unavailable
    // without hammering the verifier.
    assert!(matches!(
        e.session.fingerprint_state(),
        FingerprintState::Blocked
    ));
    assert!(e.session.model_status(SLUG).is_err());
    let msg = expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        503,
    );
    assert!(msg.contains("not_verified"));
    assert_eq!(
        e.verifier.router_calls.load(Ordering::SeqCst),
        1 + super::session::ATC_ATTEMPTS
    );
}

#[tokio::test]
async fn verify_failure_fails_closed() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    // Verifier now reports an SPKI that is not what the server presents; proxy fetch fails.
    *e.verifier.spki.lock().unwrap() = [7u8; 32];
    assert_eq!(e.session.verify_now().await, Err(TinfoilVerifyError::Fetch));
    expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        503,
    );
}

#[tokio::test]
async fn proxy_reread_picks_up_pins_miss() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    assert!(e.session.model_status(SLUG).is_ok());
    e.verifier
        .deny_models
        .lock()
        .unwrap()
        .push(SLUG.to_string());
    e.session.reread_proxy().await;
    assert_eq!(
        e.session.model_status(SLUG).unwrap_err(),
        TinfoilVerifyError::UnknownModelMeasurement
    );
    let msg = expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        503,
    );
    assert!(msg.contains("unknown_model_measurement"));
    assert_eq!(
        e.verifier.router_calls.load(Ordering::SeqCst),
        1,
        "reread does not re-verify the router"
    );
}

#[tokio::test]
async fn published_context_window_reads_models() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    assert_eq!(
        e.session
            .published_context_window("deepseek-v4-1-flash")
            .await,
        Some(1048576)
    );
    assert_eq!(e.session.published_context_window("missing").await, None);
}

#[tokio::test]
async fn attestation_report_payload_shape() {
    let e = env().await;
    // No network: install a verified state directly.
    let bundle: AtcBundle = serde_json::from_slice(&atc_json()).unwrap();
    let doc: ProxyDoc = serde_json::from_value(proxy_doc()).unwrap();
    let entry = doc.models.get(SLUG).unwrap();
    let mut models = BTreeMap::new();
    models.insert(
        SLUG.to_string(),
        Ok(PinnedModel {
            slug: SLUG.into(),
            repo: entry.repo.clone(),
            tag: entry.tag.clone(),
            entry: entry.clone(),
        }),
    );
    e.session.install_state(VerifiedState {
        router: VerifiedRouter {
            spki_sha256: [1; 32],
            measurement_hex: "ab".repeat(48),
            tag: "r@v1".into(),
        },
        models,
        verified_at: Instant::now(),
        bundle,
    });
    let m = e
        .provider
        .get_attestation_report(CANON.into(), None, Some("ff".into()), None, false)
        .await
        .unwrap();
    assert_eq!(m["provider"], "tinfoil");
    assert_eq!(m["trust"], "router_attested");
    assert_eq!(m["verified"], true);
    assert_eq!(m["model"], CANON);
    assert_eq!(m["router"]["format"], "sev-snp-guest/v2");
    assert_eq!(m["router"]["measurement"], "ab".repeat(48));
    assert_eq!(m["router"]["tag"], "r@v1");
    assert_eq!(m["router"]["report_b64"], "cmVwb3J0");
    assert_eq!(m["model_entry"]["slug"], SLUG);
    assert_eq!(
        m["model_entry"]["registers"],
        serde_json::json!(["aa", "bb"])
    );
    assert_eq!(m["model_entry"]["replicas"][0]["host"], "h1.example");
    assert!(
        m.get("nonce").is_none(),
        "client nonce is not bound into the Tinfoil report"
    );
}

#[tokio::test]
async fn attestation_report_unverified_is_error() {
    let e = env().await;
    assert!(e
        .provider
        .get_attestation_report(CANON.into(), None, None, None, false)
        .await
        .is_err());
}

#[tokio::test]
async fn trait_surface() {
    let e = env().await;
    let p = &e.provider;
    assert_eq!(p.tier(), crate::ProviderTier::Attested3p);
    assert_eq!(p.provider_source(), crate::ProviderSource::Tinfoil);
    assert!(!p.supports_chat_signatures());
    assert!(!p.supports_client_e2ee());
    assert!(!p.supports_per_request_pubkey_routing("x"));
    assert!(p.supports_streaming());
    let m = p.models().await.unwrap();
    assert_eq!(m.data[0].id, CANON);
}
