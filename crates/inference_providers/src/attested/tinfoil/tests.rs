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
    // Not the caller's fault: fall through to the next provider.
    for st in [404, 408, 425] {
        assert_eq!(map_upstream_status(st), UpstreamDisposition::Retryable503);
    }
    assert_eq!(
        map_upstream_status(400),
        UpstreamDisposition::ReturnAs4xx(400)
    );
    assert_eq!(
        map_upstream_status(413),
        UpstreamDisposition::ReturnAs4xx(413)
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
        let p = rcgen::CertificateParams::new(vec![
            "127.0.0.1".to_string(),
            "router-0.tinfoil.sh".to_string(),
        ])
        .unwrap();
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
    atc_domain: Arc<Mutex<String>>,
    /// Requests served per path.
    hits: Arc<Mutex<std::collections::HashMap<String, usize>>>,
    /// While set, `/attestation` never answers (a stalled ATC).
    atc_stall: Arc<std::sync::atomic::AtomicBool>,
    /// Delay before the chat reply is sent, in ms.
    chat_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    /// While set, the chat reply sends its headers and then stalls the body.
    chat_stall_body: Arc<std::sync::atomic::AtomicBool>,
    /// Replaces the `/v1/models` body when set.
    models_body: Arc<Mutex<Option<Vec<u8>>>>,
}

impl TestServer {
    fn hits(&self, path: &str) -> usize {
        self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }
}

async fn start_server(pki: &TestPki) -> TestServer {
    let acc = Arc::new(Mutex::new(acceptor(&pki.leaf_a, &pki.key_a)));
    let chat = Arc::new(Mutex::new(ChatReply {
        status: 200,
        content_type: "application/json",
        body: fixture("chat_nonstream.json"),
    }));
    let last_chat = Arc::new(Mutex::new(None));
    let atc_domain = Arc::new(Mutex::new("inference.tinfoil.sh".to_string()));
    let hits: Arc<Mutex<std::collections::HashMap<String, usize>>> = Arc::default();
    let atc_stall = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let chat_delay_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let chat_stall_body = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let models_body: Arc<Mutex<Option<Vec<u8>>>> = Arc::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (acc2, chat2, last2) = (acc.clone(), chat.clone(), last_chat.clone());
    let domain2 = atc_domain.clone();
    let (hits2, stall2, delay2) = (hits.clone(), atc_stall.clone(), chat_delay_ms.clone());
    let (body_stall2, models2) = (chat_stall_body.clone(), models_body.clone());
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let a = acc2.lock().unwrap().clone();
            let (chat, last) = (chat2.clone(), last2.clone());
            let domain = domain2.lock().unwrap().clone();
            let (hits, stall, delay) = (hits2.clone(), stall2.clone(), delay2.clone());
            let (body_stall, models) = (body_stall2.clone(), models2.clone());
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
                *hits.lock().unwrap().entry(path.clone()).or_default() += 1;
                if path == "/attestation" && stall.load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                    return;
                }
                if path == "/v1/chat/completions" {
                    let ms = delay.load(Ordering::SeqCst);
                    if ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    }
                }
                let (status, ct, out): (u16, &str, Vec<u8>) = match path.as_str() {
                    "/attestation" => (200, "application/json", atc_json(&domain)),
                    "/.well-known/tinfoil-proxy" => (200, "application/json", proxy_json()),
                    "/v1/models" => (
                        200,
                        "application/json",
                        models
                            .lock()
                            .unwrap()
                            .clone()
                            .unwrap_or_else(|| fixture("models.json")),
                    ),
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
                if path == "/v1/chat/completions" && body_stall.load(Ordering::SeqCst) {
                    // Headers promised a body that never arrives.
                    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                    return;
                }
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
        atc_domain,
        hits,
        atc_stall,
        chat_delay_ms,
        chat_stall_body,
        models_body,
    }
}

fn atc_json(domain: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "domain": domain,
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
            tcb: RouterTcb {
                bootloader: 10,
                tee: 0,
                snp: 23,
                microcode: 84,
            },
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

/// Poll `cond` until it holds, within a real-time deadline.
async fn wait_for(mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "condition not reached"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
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
    for s in [404u16, 408, 425] {
        set(s);
        expect_http(
            e.provider
                .chat_completion(params(false, None), "h".into())
                .await,
            503,
        );
    }
    assert_eq!(e.session.upstream_auth_failures(), 3, "not auth failures");
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
    // The re-verify runs detached from the request.
    wait_for(|| e.verifier.router_calls.load(Ordering::SeqCst) == 2).await;
    wait_for(|| matches!(e.session.fingerprint_state(), FingerprintState::Blocked)).await;
    assert_eq!(
        e.verifier.router_calls.load(Ordering::SeqCst),
        2,
        "exactly one re-verify"
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
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 2);
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
        e.session.published_context_window("gpt-oss-120b"),
        Some(131072)
    );
    assert_eq!(e.session.published_context_window("missing"), None);
}

#[tokio::test]
async fn attestation_report_payload_shape() {
    let e = env().await;
    // No network: install a verified state directly.
    let bundle: AtcBundle = serde_json::from_slice(&atc_json("inference.tinfoil.sh")).unwrap();
    let doc: ProxyDoc = serde_json::from_value(proxy_doc()).unwrap();
    let entry = doc.models.get(SLUG).unwrap();
    let mut models = BTreeMap::new();
    models.insert(
        SLUG.to_string(),
        Ok(PinnedModel {
            slug: SLUG.into(),
            entry: entry.clone(),
        }),
    );
    e.session.install_state(VerifiedState {
        router: VerifiedRouter {
            spki_sha256: [1; 32],
            measurement_hex: "ab".repeat(48),
            tag: "r@v1".into(),
            tcb: RouterTcb {
                bootloader: 10,
                tee: 0,
                snp: 23,
                microcode: 84,
            },
        },
        models,
        verified_at: Instant::now(),
        context_windows: BTreeMap::new(),
        bundle,
        transport: Arc::new(
            e.session
                .build_transport("inference.tinfoil.sh", &"01".repeat(32))
                .unwrap(),
        ),
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
    assert_eq!(
        m["router"]["tcb"],
        serde_json::json!({"bootloader": 10, "tee": 0, "snp": 23, "microcode": 84})
    );
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

// ------------------------------------------------------------ attested domain

#[test]
fn router_domain_syntax() {
    use super::validate_router_domain as ok;
    assert!(ok("inference.tinfoil.sh").is_ok());
    assert!(ok("router-0.tinfoil.sh").is_ok());
    assert!(ok("a.b.tinfoil.sh").is_ok());
    for bad in [
        "https://inference.tinfoil.sh",
        "inference.tinfoil.sh:443",
        "inference.tinfoil.sh/x",
        "user@inference.tinfoil.sh",
        "inference.example.com",
        "tinfoil.sh",
        ".tinfoil.sh",
        "..tinfoil.sh",
        "Router-0.tinfoil.sh",
        "xn--rter-pta.tinfoil.sh",
        "a.xn--b.tinfoil.sh",
        "a..tinfoil.sh",
        "a.tinfoil.sh.",
        "inference.tinfoil.sh.",
        "r\u{00f6}uter.tinfoil.sh",
        "-x.tinfoil.sh",
        "x.tinfoil.sh.evil.com",
        "",
    ] {
        assert_eq!(
            ok(bad),
            Err(TinfoilVerifyError::Malformed {
                stage: "router_domain"
            }),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn bundle_domain_selects_the_request_host() {
    let pki = test_pki();
    let server = start_server(&pki).await;
    *server.atc_domain.lock().unwrap() = "router-0.tinfoil.sh".to_string();
    let cfg = Config::new("tk_test_key".into(), 5)
        .with_route(server.addr, &format!("https://{}/attestation", server.addr));
    let verifier =
        StubVerifier::new(&compute_spki_fingerprint_from_der(pki.leaf_a.as_ref()).unwrap());
    let session =
        TinfoilRouterSession::new_with_roots(cfg.clone(), verifier, pki.roots.clone()).unwrap();
    session.verify_now().await.unwrap();
    let provider = Provider::new(session.clone(), &cfg, SLUG.into(), CANON.into());
    provider
        .chat_completion(params(false, None), "h".into())
        .await
        .unwrap();
    let head = server.last_chat.lock().unwrap().clone().unwrap().0;
    assert!(
        head.to_ascii_lowercase()
            .contains("host: router-0.tinfoil.sh"),
        "{head}"
    );
    assert!(session.published_context_window(SLUG).is_some());
}

#[tokio::test]
async fn invalid_bundle_domain_is_rejected_and_closed() {
    let e = env().await;
    for bad in [
        "https://inference.tinfoil.sh",
        "inference.tinfoil.sh:443",
        "inference.example.com",
        "tinfoil.sh",
        "Inference.tinfoil.sh",
    ] {
        *e.server.atc_domain.lock().unwrap() = bad.to_string();
        assert_eq!(
            e.session.verify_now().await,
            Err(TinfoilVerifyError::Malformed {
                stage: "router_domain"
            }),
            "{bad}"
        );
        assert!(e.session.model_status(SLUG).is_err());
        assert!(matches!(
            e.session.fingerprint_state(),
            FingerprintState::Blocked
        ));
    }
}

#[tokio::test]
async fn failed_proxy_fetch_does_not_publish_the_new_pin() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    let before = e.session.fingerprint_state();
    // The new key is not what the server presents: the proxy fetch over the
    // candidate client fails, and the (closed) session never adopts the new pin.
    *e.verifier.spki.lock().unwrap() = [7u8; 32];
    assert_eq!(e.session.verify_now().await, Err(TinfoilVerifyError::Fetch));
    assert!(matches!(
        e.session.fingerprint_state(),
        FingerprintState::Blocked
    ));
    assert!(matches!(before, FingerprintState::Pinned(_)));
}

// ------------------------------------------------------- review-fix coverage

fn provider_with_timeout(e: &Env, secs: i64) -> Provider {
    let cfg = Config::new("tk_test_key".into(), secs);
    Provider::new(e.session.clone(), &cfg, SLUG.into(), CANON.into())
}

async fn rotate_to_unattested_cert(e: &Env) {
    *e.server.acceptor.lock().unwrap() = acceptor(&e.pki.leaf_b, &e.pki.key_b);
}

#[tokio::test]
async fn connect_failure_returns_503_promptly_while_the_reverify_stalls() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    rotate_to_unattested_cert(&e).await;
    e.server.atc_stall.store(true, Ordering::SeqCst);

    let started = std::time::Instant::now();
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        e.provider.chat_completion(params(false, None), "h".into()),
    )
    .await
    .expect("a connect failure must not wait for the re-verify");
    let msg = expect_http(r, 503);
    assert!(msg.contains("connect_failed"), "{msg}");
    assert!(started.elapsed() < std::time::Duration::from_secs(3));

    // The detached verify reached the (stalled) ATC exactly once; a second
    // failure while it is pending neither waits nor starts another.
    wait_for(|| e.server.hits("/attestation") == 2).await;
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        e.provider.chat_completion(params(false, None), "h".into()),
    )
    .await
    .expect("second failure must not wait either");
    expect_http(r, 503);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(e.server.hits("/attestation"), 2, "no second verify");
    assert_eq!(
        e.verifier.router_calls.load(Ordering::SeqCst),
        1,
        "the stalled verify never reached the verifier"
    );
    assert!(e.server.last_chat.lock().unwrap().is_none());
}

#[tokio::test]
async fn second_connect_failure_within_cooldown_does_not_reverify() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    let g = e.session.generation();
    e.session.on_connect_failure(g);
    wait_for(|| {
        e.verifier.router_calls.load(Ordering::SeqCst) == 2 && !e.session.reverify_pending()
    })
    .await;
    // A fresh generation, nothing pending: only the cooldown can stop this one.
    e.session.on_connect_failure(e.session.generation());
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!e.session.reverify_pending());
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 2);
    // A stale generation is deduped against the verify that already ran.
    e.session.on_connect_failure(g);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn request_timeout_maps_to_503_timeout() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    e.server.chat_delay_ms.store(3000, Ordering::SeqCst);
    let p = provider_with_timeout(&e, 1);
    let started = std::time::Instant::now();
    let msg = expect_http(
        p.chat_completion(params(false, None), "h".into()).await,
        503,
    );
    assert!(msg.contains("timeout"), "{msg}");
    assert!(started.elapsed() < std::time::Duration::from_millis(2500));
    // A timeout is not a connection failure: no re-verification.
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stalled_error_body_is_bounded_by_the_request_timeout() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    *e.server.chat.lock().unwrap() = ChatReply {
        status: 500,
        content_type: "application/json",
        body: vec![b'x'; 100],
    };
    e.server.chat_stall_body.store(true, Ordering::SeqCst);
    let p = provider_with_timeout(&e, 1);
    let started = std::time::Instant::now();
    let msg = expect_http(
        p.chat_completion(params(false, None), "h".into()).await,
        503,
    );
    assert!(msg.contains("upstream_error"), "{msg}");
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
}

#[tokio::test]
async fn oversized_error_body_is_truncated() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    let big = super::session::MAX_DOC_BYTES + 4096;
    *e.server.chat.lock().unwrap() = ChatReply {
        status: 500,
        content_type: "text/plain",
        body: vec![b'x'; big],
    };
    let snap = e.session.snapshot();
    let t = snap.as_ref().as_ref().unwrap().transport.clone();
    let resp = t
        .client
        .post(format!("{}/v1/chat/completions", t.base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    let text = super::read_capped_text(resp).await;
    assert_eq!(text.len(), super::session::MAX_DOC_BYTES);
    // The same cap through the provider: a 4xx surfaces, bounded.
    *e.server.chat.lock().unwrap() = ChatReply {
        status: 400,
        content_type: "text/plain",
        body: vec![b'y'; big],
    };
    match e
        .provider
        .chat_completion(params(false, None), "h".into())
        .await
    {
        Err(CompletionError::HttpError {
            status_code: 400,
            message,
            ..
        }) => assert!(message.len() <= super::session::MAX_DOC_BYTES + 256),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn oversized_models_document_is_rejected_without_failing_the_verify() {
    let e = env().await;
    *e.server.models_body.lock().unwrap() = Some(vec![b' '; super::session::MAX_DOC_BYTES + 1]);
    e.session.verify_now().await.unwrap();
    assert!(e.session.model_status(SLUG).is_ok());
    assert_eq!(e.session.published_context_window(SLUG), None);
}

#[tokio::test]
async fn client_e2ee_pubkey_is_rejected_before_any_upstream_request() {
    use crate::attested::nearai::encryption_headers as eh;
    let e = env().await;
    e.session.verify_now().await.unwrap();
    let mk = |stream| {
        let mut p = params(stream, None);
        p.extra
            .insert(eh::CLIENT_PUB_KEY.to_string(), serde_json::json!("abcd"));
        p
    };
    for result in [
        e.provider
            .chat_completion(mk(false), "h".into())
            .await
            .map(|_| ()),
        e.provider
            .chat_completion_stream(mk(true), "h".into())
            .await
            .map(|_| ()),
    ] {
        match result {
            Err(CompletionError::CompletionError(m)) => assert!(m.contains("E2EE"), "{m}"),
            other => panic!("expected an E2EE rejection, got {other:?}"),
        }
    }
    assert_eq!(e.server.hits("/v1/chat/completions"), 0);
    assert!(e.server.last_chat.lock().unwrap().is_none());
}

#[tokio::test]
async fn continuous_usage_stats_alone_counts_as_asking_for_usage() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    *e.server.chat.lock().unwrap() = ChatReply {
        status: 200,
        content_type: "text/event-stream",
        body: fixture("chat_stream.sse"),
    };
    let mut p = params(true, None);
    p.stream_options =
        Some(serde_json::from_value(serde_json::json!({"continuous_usage_stats": true})).unwrap());
    let s = e
        .provider
        .chat_completion_stream(p, "h".into())
        .await
        .unwrap();
    let evs: Vec<_> = s.map(|x| x.unwrap()).collect().await;
    let usage_chunks = evs
        .iter()
        .filter_map(client_json)
        .filter(|v| v.get("usage").is_some())
        .count();
    assert_eq!(usage_chunks, 1, "the final usage chunk reaches the client");
}

#[tokio::test]
async fn proxy_reread_fetch_failure_escalates_to_full_verify_and_fails_closed() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 1);
    rotate_to_unattested_cert(&e).await;
    e.session.reread_proxy().await;
    assert_eq!(
        e.verifier.router_calls.load(Ordering::SeqCst),
        2,
        "the failed re-read escalated to a full verification"
    );
    assert!(matches!(
        e.session.fingerprint_state(),
        FingerprintState::Blocked
    ));
    assert!(e.session.model_status(SLUG).is_err());
    assert_eq!(
        e.session.last_verify_error(),
        Some(TinfoilVerifyError::Fetch)
    );
}

#[tokio::test]
async fn reread_proxy_when_unverified_runs_full_verify() {
    let e = env().await;
    assert!(e.session.model_status(SLUG).is_err());
    e.session.reread_proxy().await;
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 1);
    assert!(e.session.model_status(SLUG).is_ok());
}

#[tokio::test]
async fn declared_ctx_above_the_published_window_fails_closed() {
    let e = env().await;
    // Registered while unverified: the provider exists, the check applies later.
    let cfg = Config::new("tk_test_key".into(), 5);
    let over = Provider::new(e.session.clone(), &cfg, SLUG.into(), CANON.into())
        .with_declared_ctx(200_000);
    let fits = Provider::new(e.session.clone(), &cfg, SLUG.into(), CANON.into())
        .with_declared_ctx(131_072);
    let msg = expect_http(
        over.chat_completion(params(false, None), "h".into()).await,
        503,
    );
    assert!(msg.contains("not_verified"), "{msg}");

    e.session.verify_now().await.unwrap();
    for _ in 0..2 {
        let msg = expect_http(
            over.chat_completion(params(false, None), "h".into()).await,
            503,
        );
        assert!(msg.contains("ctx_exceeds_published"), "{msg}");
    }
    expect_http(
        over.chat_completion_stream(params(true, None), "h".into())
            .await,
        503,
    );
    assert!(
        e.server.last_chat.lock().unwrap().is_none(),
        "no request left the gateway for the oversized declaration"
    );
    fits.chat_completion(params(false, None), "h".into())
        .await
        .unwrap();
    assert!(e.server.last_chat.lock().unwrap().is_some());
}

#[tokio::test]
async fn unknown_published_window_leaves_the_declared_ctx_standing() {
    let e = env().await;
    *e.server.models_body.lock().unwrap() = Some(b"{}".to_vec());
    e.session.verify_now().await.unwrap();
    let cfg = Config::new("tk_test_key".into(), 5);
    Provider::new(e.session.clone(), &cfg, SLUG.into(), CANON.into())
        .with_declared_ctx(200_000)
        .chat_completion(params(false, None), "h".into())
        .await
        .unwrap();
}

#[tokio::test]
async fn spawn_refresh_rereads_proxy_then_reverifies_and_stops_when_dropped() {
    use std::time::Duration;
    let Env {
        session,
        verifier,
        server,
        provider,
        pki: _pki,
    } = env().await;
    session.verify_now().await.unwrap();
    // Pause only after the real-network setup: auto-advance during a live TLS
    // handshake would trip the fetch timeouts.
    tokio::time::pause();
    let proxy_path = "/.well-known/tinfoil-proxy";
    let proxy0 = server.hits(proxy_path);
    assert_eq!(verifier.router_calls.load(Ordering::SeqCst), 1);

    let rt = tokio::runtime::Handle::current();
    let before = rt.metrics().num_alive_tasks();
    session.spawn_refresh();
    let with_task = rt.metrics().num_alive_tasks();
    assert_eq!(with_task, before + 1);
    session.spawn_refresh();
    assert_eq!(
        rt.metrics().num_alive_tasks(),
        with_task,
        "a second spawn_refresh adds no task"
    );

    // Let the task create its intervals (at the paused "now") before advancing.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    // PROXY_REREAD: the model document is re-read, the router is not re-verified.
    tokio::time::advance(super::PROXY_REREAD + Duration::from_secs(1)).await;
    paused_wait(|| server.hits(proxy_path) > proxy0).await;
    assert_eq!(verifier.router_calls.load(Ordering::SeqCst), 1);

    // ROUTER_REVERIFY: a full verification runs.
    tokio::time::advance(super::ROUTER_REVERIFY).await;
    paused_wait(|| verifier.router_calls.load(Ordering::SeqCst) >= 2).await;

    // Dropping the last strong reference ends the task.
    drop(provider);
    drop(session);
    paused_wait(|| rt.metrics().num_alive_tasks() < with_task).await;
}

/// Wait for `cond` under a paused clock. Short sleeps (not `yield_now`, which
/// starves the IO driver) let loopback IO progress; the clock only auto-advances
/// when the runtime is idle, in 1 ms steps here.
async fn paused_wait(mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "condition not reached"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

async fn map_raw_sse(raw: &str) -> Vec<Result<crate::SSEEvent, crate::CompletionError>> {
    let s = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(
        raw.to_string(),
    ))]);
    let parsed: crate::StreamingResult =
        Box::pin(crate::sse_parser::new_external_sse_parser(s, true));
    wire::map_stream(parsed, CANON.to_string(), false)
        .collect()
        .await
}

const LEAKY: &str = r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"upstream-slug","prompt_text":"SECRET","prompt_token_ids":[1,2],"choices":[{"index":0,"token_ids":[9],"delta":{"content":"hi","token_ids":[9]}}]}"#;

#[tokio::test]
async fn data_frame_without_space_is_sanitized_not_passed_through() {
    let out = map_raw_sse(&format!("data:{LEAKY}\n\ndata: [DONE]\n\n")).await;
    let mut saw_chunk = false;
    for ev in out {
        let ev = ev.unwrap();
        let raw = String::from_utf8_lossy(&ev.raw_bytes).to_string();
        for k in ["prompt_text", "SECRET", "token_ids", "upstream-slug"] {
            assert!(!raw.contains(k), "{k} leaked: {raw}");
        }
        if let Some(v) = client_json(&ev) {
            assert_eq!(v["model"], CANON);
            saw_chunk = true;
        }
    }
    assert!(saw_chunk);
}

#[tokio::test]
async fn data_done_without_space_still_terminates() {
    let out = map_raw_sse("data:[DONE]\n\n").await;
    assert!(out.into_iter().any(|e| e.unwrap().is_done_marker()));
}

#[tokio::test]
async fn unknown_lines_are_never_forwarded() {
    for line in [
        format!("event: {LEAKY}\n\n"),
        format!("id: {LEAKY}\n\n"),
        format!("{LEAKY}\n\n"),
        format!("retry: 5 {LEAKY}\n\n"),
    ] {
        let out = map_raw_sse(&line).await;
        assert!(out.iter().any(|r| r.is_err()), "not rejected: {line}");
        for ev in out.iter().flatten() {
            let raw = String::from_utf8_lossy(&ev.raw_bytes);
            assert!(!raw.contains("SECRET"), "unknown line forwarded: {raw}");
        }
    }
}

#[tokio::test]
async fn blank_and_comment_lines_still_pass_through() {
    let out = map_raw_sse(": keepalive\n\n").await;
    assert!(!out.is_empty() && out.iter().all(|r| r.is_ok()));
}
