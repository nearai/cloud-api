//! Tests for the Tinfoil router session and provider. The TLS server here is a
//! local stand-in for `inference.tinfoil.sh`; nothing talks to the live API.

mod mapping;
mod provider;
mod session;

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

fn client_json(ev: &crate::SSEEvent) -> Option<serde_json::Value> {
    let s = std::str::from_utf8(&ev.raw_bytes).ok()?;
    let d = s.trim().strip_prefix("data:")?.trim();
    serde_json::from_str(d).ok()
}
