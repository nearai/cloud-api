//! Network side of the sync: fetch the router's ATC bundle, observe the router,
//! fetch the proxy document from that attested router, and collect Sigstore
//! attestations for every release in play.
//!
//! The ATC bundle is fetched over ordinary TLS but is self-authenticating: it
//! is verified (SEV-SNP chain, report signature, TCB, debug bit, binding to the
//! router's TLS key) before anything else is trusted. The proxy document is
//! then fetched only from the verified bundle's domain, over a client that
//! pins the TLS key the SNP report attests, so its model rows were published by
//! the attested router and not by whoever can obtain a WebPKI certificate for
//! that name. If the router does not verify, the proxy document is not fetched.
//!
//! Only public endpoints are used and no Tinfoil key. Errors are reported by
//! HTTP status or category only, never with the upstream body.

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use inference_providers::spki_verifier::{
    canonical_spki_fingerprint, FingerprintState, SharedTlsRoots,
};

use inference_providers::attested::tinfoil::verifier_port::{
    validate_router_domain, AtcBundle, ProxyDoc,
};
use services::attestation::tinfoil_observer::observe;

use crate::classify::{
    ModelObservation, Observations, RouterObservation, SigstoreResults, ROUTER_REPO,
};
use crate::sigstore_verify::verify_bundle;

pub const ATC_URL: &str = "https://atc.tinfoil.sh/attestation";
/// Path of the proxy document on the attested router's domain.
pub const PROXY_PATH: &str = "/.well-known/tinfoil-proxy";
pub const GITHUB_DOWNLOAD_BASE: &str = "https://github.com";
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// Largest response body accepted from any endpoint (the ATC bundle and proxy
/// document are well under 1 MiB).
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;

/// Most distinct repo/tag releases a proxy document may list before it is
/// rejected unchecked; Tinfoil serves roughly 10-20 models today, so this is
/// generous headroom while bounding the per-release fetches a hostile or
/// compromised router can trigger.
const MAX_RELEASES: usize = 64;

/// Who the proxy document is requested from: the router a verified ATC bundle
/// describes.
#[derive(Debug, Clone)]
pub struct ProxyTarget {
    /// Domain from the verified bundle (a plain hostname).
    pub domain: String,
    /// SHA-256 of the router's TLS SPKI as attested by the SNP report.
    pub spki_sha256_hex: String,
}

/// Replaces the pinned proxy fetch. Test seam only: the binary never sets it,
/// and with it unset the proxy document is only ever fetched through the
/// SPKI-pinned client.
pub type ProxyFetcher = Arc<
    dyn Fn(ProxyTarget) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

/// Network settings of the pinned proxy fetch. Both fields exist so tests can
/// reach a local TLS server; neither weakens the pin.
#[derive(Default, Clone)]
pub struct ProxyNet {
    /// Trust roots for the WebPKI check (default: the native roots). The SPKI
    /// pin is checked in addition.
    pub roots: Option<SharedTlsRoots>,
    /// Connect to this address instead of resolving the domain (the URL keeps
    /// the verified domain and takes this address's port).
    pub connect_to: Option<SocketAddr>,
}

pub struct ProbeConfig {
    pub atc_url: String,
    /// Path requested on the attested router's domain.
    pub proxy_path: String,
    pub proxy_net: ProxyNet,
    pub proxy_fetcher: Option<ProxyFetcher>,
    /// Where `<repo>/releases/download/<tag>/tinfoil.hash` is served.
    pub download_base: String,
    /// Where `repos/<repo>/attestations/sha256:<digest>` is served.
    pub api_base: String,
    /// Optional GitHub token; raises the attestations API rate limit.
    pub github_token: Option<String>,
    /// Attempts per call (first try included) for transport errors, 429 and 5xx.
    pub attempts: u32,
    /// Backoff before attempt `n + 1` is `backoff * n`.
    pub backoff: Duration,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            atc_url: ATC_URL.into(),
            proxy_path: PROXY_PATH.into(),
            proxy_net: ProxyNet::default(),
            proxy_fetcher: None,
            download_base: GITHUB_DOWNLOAD_BASE.into(),
            api_base: GITHUB_API_BASE.into(),
            github_token: None,
            attempts: 3,
            backoff: Duration::from_secs(5),
        }
    }
}

#[derive(Debug)]
pub struct ProbeOutput {
    pub observations: Observations,
    pub sigstore: SigstoreResults,
    /// Why something was not observed or verified. Categories only.
    pub notes: Vec<String>,
    /// The router verified and its proxy document was fetched and decoded. A
    /// run without both has nothing to sync and must not look like a success.
    pub complete: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("{0}")]
    Fetch(String),
}

/// Reads a response body, failing once it exceeds [`MAX_BODY_BYTES`].
async fn read_capped(mut r: reqwest::Response) -> Result<String, String> {
    if r.content_length().is_some_and(|n| n > MAX_BODY_BYTES) {
        return Err("body too large".into());
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = r.chunk().await.map_err(|_| "body read error".to_string())? {
        if buf.len() as u64 + chunk.len() as u64 > MAX_BODY_BYTES {
            return Err("body too large".into());
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| "body not utf-8".to_string())
}

async fn get_text(
    client: &reqwest::Client,
    cfg: &ProbeConfig,
    url: &str,
    bearer: Option<&str>,
) -> Result<String, String> {
    let mut last = String::from("no attempt");
    for n in 1..=cfg.attempts.max(1) {
        if n > 1 {
            tokio::time::sleep(cfg.backoff * (n - 1)).await;
        }
        let mut req = client
            .get(url)
            .header("user-agent", "tinfoil-measurement-sync")
            .header("accept", "application/json, text/plain");
        if let Some(t) = bearer {
            req = req.bearer_auth(t);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => return read_capped(r).await,
            Ok(r) => {
                let s = r.status();
                last = format!("HTTP {}", s.as_u16());
                if !(s.as_u16() == 429 || s.is_server_error()) {
                    break;
                }
            }
            Err(_) => last = "transport error".into(),
        }
    }
    Err(last)
}

/// `tinfoilsh/<name>` with a conservative name: it is placed in URLs.
fn is_tinfoil_repo(repo: &str) -> bool {
    repo.strip_prefix("tinfoilsh/").is_some_and(|name| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    })
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The verified Sigstore attestation of `repo@tag`, from the release's
/// `tinfoil.hash` (digest of `tinfoil-deployment.json`) and GitHub's
/// attestations API.
async fn fetch_release_attestation(
    client: &reqwest::Client,
    cfg: &ProbeConfig,
    repo: &str,
    tag: &str,
) -> Result<crate::sigstore_verify::SigstoreResult, String> {
    if !is_tinfoil_repo(repo) {
        return Err("repo outside tinfoilsh".into());
    }
    if tag.is_empty()
        || !tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
    {
        return Err("malformed tag".into());
    }
    let hash_url = format!(
        "{}/{repo}/releases/download/{tag}/tinfoil.hash",
        cfg.download_base
    );
    let digest = get_text(client, cfg, &hash_url, None)
        .await
        .map_err(|e| format!("tinfoil.hash: {e}"))?
        .trim()
        .to_ascii_lowercase();
    if !is_sha256_hex(&digest) {
        return Err("tinfoil.hash: malformed".into());
    }
    let url = format!("{}/repos/{repo}/attestations/sha256:{digest}", cfg.api_base);
    let body = get_text(client, cfg, &url, cfg.github_token.as_deref())
        .await
        .map_err(|e| format!("attestations: {e}"))?;
    let doc: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| "attestations: not JSON".to_string())?;
    let bundles = doc["attestations"]
        .as_array()
        .ok_or("attestations: no list")?;
    for a in bundles {
        if let Ok(r) = verify_bundle(&a["bundle"], repo) {
            if r.subject_sha256 == digest && r.tag == tag {
                return Ok(r);
            }
        }
    }
    Err("no attestation verified for this release".into())
}

/// An HTTP client whose every connection must present a certificate that both
/// passes WebPKI for the connected name and has exactly the attested SPKI.
/// Redirects are not followed, and TLS session resumption is off (see
/// `SharedTlsRoots::build_config`), so each connection is checked.
fn pinned_client(net: &ProxyNet, target: &ProxyTarget) -> Result<reqwest::Client, String> {
    let roots = net.roots.clone().unwrap_or_else(SharedTlsRoots::load);
    let pin = canonical_spki_fingerprint(&target.spki_sha256_hex);
    let state = Arc::new(RwLock::new(FingerprintState::Pinned(HashSet::from([pin]))));
    let mut b = reqwest::Client::builder()
        .use_preconfigured_tls(roots.build_config(state))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30));
    if let Some(addr) = net.connect_to {
        b = b.resolve(&target.domain, addr);
    }
    b.build().map_err(|_| "pinned client".to_string())
}

/// The proxy document, fetched from the attested router's domain through a
/// client pinned to its attested TLS key.
async fn fetch_proxy_pinned(cfg: &ProbeConfig, target: &ProxyTarget) -> Result<String, String> {
    if validate_router_domain(&target.domain).is_err() {
        return Err("attested domain is not a hostname".into());
    }
    let client = pinned_client(&cfg.proxy_net, target)?;
    let port = cfg
        .proxy_net
        .connect_to
        .map(|a| format!(":{}", a.port()))
        .unwrap_or_default();
    let url = format!("https://{}{port}{}", target.domain, cfg.proxy_path);
    get_text(&client, cfg, &url, None).await
}

pub async fn run(client: &reqwest::Client, cfg: &ProbeConfig) -> Result<ProbeOutput, ProbeError> {
    let mut notes = Vec::new();
    let atc_text = get_text(client, cfg, &cfg.atc_url, None)
        .await
        .map_err(|e| ProbeError::Fetch(format!("ATC bundle: {e}")))?;
    let bundle: AtcBundle = serde_json::from_str(&atc_text)
        .map_err(|_| ProbeError::Fetch("ATC bundle: malformed".into()))?;

    let observed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut observations = Observations {
        observed_at,
        ..Observations::default()
    };
    let mut sigstore = SigstoreResults::default();

    // Verify the router first: nothing the proxy document says is trusted
    // unless it came from this attested router.
    let mut target = None;
    match observe(&bundle) {
        Ok(o) => {
            target = Some(ProxyTarget {
                domain: bundle.domain.clone(),
                spki_sha256_hex: o.spki_sha256_hex.clone(),
            });
            observations.router = Some(RouterObservation {
                measurement_hex: o.measurement_hex,
                spki_sha256_hex: o.spki_sha256_hex,
                tcb: format!(
                    "{}/{}/{}/{}",
                    o.tcb.bootloader, o.tcb.tee, o.tcb.snp, o.tcb.microcode
                ),
                format: o.format,
            });
            match verify_bundle(&bundle.sigstore_bundle, ROUTER_REPO) {
                Ok(r) if r.subject_sha256.eq_ignore_ascii_case(&bundle.digest) => {
                    sigstore.router = Some(r)
                }
                Ok(_) => notes.push("router: sigstore subject differs from ATC digest".into()),
                Err(e) => notes.push(format!("router: {e}")),
            }
        }
        Err(e) => notes.push(format!("router: observation failed: {}", e.reason())),
    }

    let proxy = match target {
        None => {
            notes.push("proxy document: not fetched, router attestation not verified".into());
            None
        }
        Some(target) => {
            let fetched = match &cfg.proxy_fetcher {
                Some(f) => f(target).await,
                None => fetch_proxy_pinned(cfg, &target).await,
            };
            match fetched {
                Ok(text) => match serde_json::from_str::<ProxyDoc>(&text) {
                    Ok(p) => {
                        let distinct = p
                            .models
                            .values()
                            .map(|m| (&m.repo, &m.tag))
                            .collect::<std::collections::BTreeSet<_>>()
                            .len();
                        if distinct > MAX_RELEASES {
                            notes.push(format!(
                                "proxy document: {distinct} distinct releases exceeds limit of {MAX_RELEASES}"
                            ));
                            None
                        } else {
                            Some(p)
                        }
                    }
                    Err(_) => {
                        notes.push("proxy document: malformed".into());
                        None
                    }
                },
                Err(e) => {
                    notes.push(format!("proxy document: {e}"));
                    None
                }
            }
        }
    };
    let complete = proxy.is_some();
    let proxy = proxy.unwrap_or(ProxyDoc {
        models: BTreeMap::new(),
    });

    let mut releases: BTreeMap<(String, String), ()> = BTreeMap::new();
    for (slug, m) in &proxy.models {
        observations.models.push(ModelObservation {
            slug: slug.clone(),
            repo: m.repo.clone(),
            tag: m.tag.clone(),
            kind: m.measurement.kind.clone(),
            registers: m.measurement.registers.clone(),
        });
        releases.insert((m.repo.clone(), m.tag.clone()), ());
    }
    for (repo, tag) in releases.into_keys() {
        match fetch_release_attestation(client, cfg, &repo, &tag).await {
            Ok(r) => {
                sigstore.models.insert(format!("{repo}@{tag}"), r);
            }
            Err(e) => notes.push(format!("{repo}@{tag}: {e}")),
        }
    }
    Ok(ProbeOutput {
        observations,
        sigstore,
        notes,
        complete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ATC: &str = include_str!("../testdata/atc_attestation.out");
    const PROXY: &str = include_str!("../testdata/proxy.json");
    const MODEL_BUNDLE: &str = include_str!("../testdata/model_sigstore_bundle.json");
    const DEEPSEEK_DIGEST: &str =
        "8448fed68f4ed10c829a6433bdf570fa12a7b0e6d589bf1a89a8e9a5ca37a3ac";

    async fn server() -> (MockServer, ProbeConfig) {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/atc"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ATC))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/proxy"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PROXY))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/tinfoilsh/confidential-deepseek-v4-1-flash/releases/download/v0.0.3/tinfoil.hash",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(format!("{DEEPSEEK_DIGEST}\n")),
            )
            .mount(&s)
            .await;
        let att = format!(r#"{{"attestations":[{{"bundle":{MODEL_BUNDLE}}}]}}"#);
        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/tinfoilsh/confidential-deepseek-v4-1-flash/attestations/sha256:{DEEPSEEK_DIGEST}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_string(att))
            .mount(&s)
            .await;
        let cfg = ProbeConfig {
            atc_url: format!("{}/atc", s.uri()),
            proxy_fetcher: Some(mock_proxy_fetcher(format!("{}/proxy", s.uri()))),
            download_base: s.uri(),
            api_base: s.uri(),
            attempts: 1,
            backoff: Duration::ZERO,
            ..ProbeConfig::default()
        };
        (s, cfg)
    }

    #[tokio::test]
    async fn probe_observes_router_and_verifies_sigstore() {
        let (_s, cfg) = server().await;
        let out = run(&reqwest::Client::new(), &cfg).await.unwrap();
        let router = out.observations.router.expect("router observed");
        assert_eq!(
            router.measurement_hex,
            out.sigstore.router.as_ref().unwrap().snp_measurement
        );
        let proxy: ProxyDoc = serde_json::from_str(PROXY).unwrap();
        assert_eq!(out.observations.models.len(), proxy.models.len());
        assert!(out
            .sigstore
            .models
            .contains_key("tinfoilsh/confidential-deepseek-v4-1-flash@v0.0.3"));
        // Every other release has no mocked attestation.
        assert_eq!(out.sigstore.models.len(), 1);
        let unmocked = proxy
            .models
            .values()
            .find(|m| m.repo != "tinfoilsh/confidential-deepseek-v4-1-flash")
            .expect("fixture has a second release");
        let key = format!("{}@{}", unmocked.repo, unmocked.tag);
        assert!(out.notes.iter().any(|n| n.contains(&key)), "{key}");
    }

    #[tokio::test]
    async fn router_sigstore_digest_mismatch_is_noted_not_pinned() {
        let (s, mut cfg) = server().await;
        let mut atc: serde_json::Value = serde_json::from_str(ATC).unwrap();
        atc["digest"] = serde_json::json!("00".repeat(32));
        Mock::given(method("GET"))
            .and(path("/atc-mismatch"))
            .respond_with(ResponseTemplate::new(200).set_body_string(atc.to_string()))
            .mount(&s)
            .await;
        cfg.atc_url = format!("{}/atc-mismatch", s.uri());
        let out = run(&reqwest::Client::new(), &cfg).await.unwrap();
        assert!(out.observations.router.is_some());
        assert!(out.sigstore.router.is_none());
        assert!(out
            .notes
            .iter()
            .any(|n| n == "router: sigstore subject differs from ATC digest"));
    }

    #[test]
    fn services_and_sync_atc_captures_agree() {
        let a: AtcBundle = serde_json::from_str(ATC).unwrap();
        let b: AtcBundle = serde_json::from_str(include_str!(
            "../../services/src/attestation/testdata/tinfoil/atc_bundle.json"
        ))
        .unwrap();
        let (oa, ob) = (observe(&a).unwrap(), observe(&b).unwrap());
        assert_eq!(oa.measurement_hex, ob.measurement_hex);
        assert_eq!(oa.spki_sha256_hex, ob.spki_sha256_hex);
    }

    fn counting_cfg(s: &MockServer, attempts: u32) -> ProbeConfig {
        ProbeConfig {
            atc_url: format!("{}/atc", s.uri()),
            download_base: s.uri(),
            api_base: s.uri(),
            attempts,
            backoff: Duration::ZERO,
            ..ProbeConfig::default()
        }
    }

    /// Serves the proxy document from a plain-HTTP mock, ignoring the target.
    /// This stands in for the pinned fetch in tests of everything after it; the
    /// pinned fetch itself is tested against a local TLS server below.
    fn mock_proxy_fetcher(url: String) -> ProxyFetcher {
        Arc::new(move |_target| {
            let url = url.clone();
            Box::pin(async move {
                let cfg = ProbeConfig {
                    attempts: 1,
                    ..ProbeConfig::default()
                };
                get_text(&reqwest::Client::new(), &cfg, &url, None).await
            })
        })
    }

    #[tokio::test]
    async fn retries_on_503_then_succeeds() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/flaky"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .expect(2)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/flaky"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&s)
            .await;
        let cfg = counting_cfg(&s, 3);
        let body = get_text(
            &reqwest::Client::new(),
            &cfg,
            &format!("{}/flaky", s.uri()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(body, "ok");
        s.verify().await;
    }

    #[tokio::test]
    async fn does_not_retry_404() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/gone"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&s)
            .await;
        let cfg = counting_cfg(&s, 3);
        let e = get_text(
            &reqwest::Client::new(),
            &cfg,
            &format!("{}/gone", s.uri()),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(e, "HTTP 404");
        s.verify().await;
    }

    #[tokio::test]
    async fn gives_up_after_attempts_on_persistent_5xx() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/down"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3)
            .mount(&s)
            .await;
        let cfg = counting_cfg(&s, 3);
        let e = get_text(
            &reqwest::Client::new(),
            &cfg,
            &format!("{}/down", s.uri()),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(e, "HTTP 500");
        s.verify().await;
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![
                b'a';
                MAX_BODY_BYTES as usize
                    + 1
            ]))
            .mount(&s)
            .await;
        let cfg = counting_cfg(&s, 1);
        let e = get_text(
            &reqwest::Client::new(),
            &cfg,
            &format!("{}/big", s.uri()),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(e, "body too large");
    }

    #[tokio::test]
    async fn bundle_for_other_tag_is_rejected() {
        let (s, cfg) = server().await;
        // Same repo, same digest, but the verified bundle is for v0.0.3.
        let repo = "tinfoilsh/confidential-deepseek-v4-1-flash";
        Mock::given(method("GET"))
            .and(path(format!(
                "/{repo}/releases/download/v0.0.4/tinfoil.hash"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_string(DEEPSEEK_DIGEST))
            .mount(&s)
            .await;
        let e = fetch_release_attestation(&reqwest::Client::new(), &cfg, repo, "v0.0.4")
            .await
            .unwrap_err();
        assert_eq!(e, "no attestation verified for this release");
        // Control: the right tag verifies.
        assert!(
            fetch_release_attestation(&reqwest::Client::new(), &cfg, repo, "v0.0.3")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn malformed_tag_is_rejected_before_request() {
        let s = MockServer::start().await;
        let cfg = counting_cfg(&s, 1);
        for tag in ["", "v1/../x", "v1?x=y", "v 1"] {
            let e = fetch_release_attestation(
                &reqwest::Client::new(),
                &cfg,
                "tinfoilsh/confidential-x",
                tag,
            )
            .await
            .unwrap_err();
            assert_eq!(e, "malformed tag", "{tag:?}");
        }
        assert!(s.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_tinfoil_hash_is_rejected() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/tinfoilsh/confidential-x/releases/download/v1/tinfoil.hash",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-a-digest"))
            .mount(&s)
            .await;
        let cfg = counting_cfg(&s, 1);
        let e = fetch_release_attestation(
            &reqwest::Client::new(),
            &cfg,
            "tinfoilsh/confidential-x",
            "v1",
        )
        .await
        .unwrap_err();
        assert_eq!(e, "tinfoil.hash: malformed");
        // Only the hash was requested; no attestations call followed.
        assert_eq!(s.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unreachable_atc_fails_the_probe() {
        let (_s, mut cfg) = server().await;
        cfg.atc_url = format!("{}/missing", cfg.api_base);
        let e = run(&reqwest::Client::new(), &cfg).await.unwrap_err();
        assert!(e.to_string().contains("ATC bundle: HTTP 404"));
    }

    #[test]
    fn repo_names_are_restricted_before_use_in_urls() {
        assert!(is_tinfoil_repo("tinfoilsh/confidential-gpt-oss-120b"));
        assert!(!is_tinfoil_repo("evil/confidential-x"));
        assert!(!is_tinfoil_repo("tinfoilsh/../x/y"));
        assert!(!is_tinfoil_repo("tinfoilsh/"));
    }

    mod pinned {
        use super::*;
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const DOMAIN: &str = "inference.tinfoil.sh";

        struct Server {
            addr: SocketAddr,
            roots: SharedTlsRoots,
            leaf_spki_hex: String,
            requests: Arc<AtomicUsize>,
        }

        /// A local CA, and a TLS server whose leaf certificate is valid for
        /// [`DOMAIN`] and answers every request with `body`.
        async fn tls_server(body: &'static str) -> Server {
            let ca_key = rcgen::KeyPair::generate().unwrap();
            let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
            ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            ca_params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
            let ca_cert = ca_params.self_signed(&ca_key).unwrap();
            let issuer = rcgen::Issuer::new(ca_params, ca_key);
            let leaf_key = rcgen::KeyPair::generate().unwrap();
            let leaf = rcgen::CertificateParams::new(vec![DOMAIN.to_string()])
                .unwrap()
                .signed_by(&leaf_key, &issuer)
                .unwrap();
            let leaf_spki_hex =
                inference_providers::spki_verifier::compute_spki_fingerprint_from_der(leaf.der())
                    .unwrap();
            let mut store = rustls::RootCertStore::empty();
            store.add(ca_cert.der().clone()).unwrap();

            let config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
            )
            .unwrap();
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(AtomicUsize::new(0));
            let counter = requests.clone();
            tokio::spawn(async move {
                while let Ok((tcp, _)) = listener.accept().await {
                    let acceptor = acceptor.clone();
                    let counter = counter.clone();
                    tokio::spawn(async move {
                        let Ok(mut tls) = acceptor.accept(tcp).await else {
                            return;
                        };
                        let mut head = Vec::new();
                        let mut chunk = [0u8; 1024];
                        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                            match tls.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => head.extend_from_slice(&chunk[..n]),
                            }
                        }
                        counter.fetch_add(1, Ordering::SeqCst);
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = tls.write_all(resp.as_bytes()).await;
                        let _ = tls.shutdown().await;
                    });
                }
            });
            Server {
                addr,
                roots: SharedTlsRoots::from_root_store(store),
                leaf_spki_hex,
                requests,
            }
        }

        fn cfg_for(s: &Server) -> ProbeConfig {
            ProbeConfig {
                proxy_net: ProxyNet {
                    roots: Some(s.roots.clone()),
                    connect_to: Some(s.addr),
                },
                attempts: 1,
                backoff: Duration::ZERO,
                ..ProbeConfig::default()
            }
        }

        fn target(spki: &str) -> ProxyTarget {
            ProxyTarget {
                domain: DOMAIN.into(),
                spki_sha256_hex: spki.into(),
            }
        }

        #[tokio::test]
        async fn proxy_is_fetched_when_the_peer_has_the_attested_key() {
            let s = tls_server("proxy-body").await;
            let got = fetch_proxy_pinned(&cfg_for(&s), &target(&s.leaf_spki_hex)).await;
            assert_eq!(got.unwrap(), "proxy-body");
            // Report encodings (upper case, 0x prefix) pin the same key.
            let upper = format!("0x{}", s.leaf_spki_hex.to_uppercase());
            assert!(fetch_proxy_pinned(&cfg_for(&s), &target(&upper))
                .await
                .is_ok());
        }

        #[tokio::test]
        async fn proxy_is_not_fetched_from_a_peer_with_another_key() {
            // The peer's certificate is valid for the name under the trusted
            // roots, but its key is not the attested one.
            let s = tls_server("forged").await;
            let wrong = "00".repeat(32);
            let got = fetch_proxy_pinned(&cfg_for(&s), &target(&wrong)).await;
            assert_eq!(got.unwrap_err(), "transport error");
            assert_eq!(
                s.requests.load(Ordering::SeqCst),
                0,
                "no request may reach a peer that fails the pin"
            );
        }

        #[tokio::test]
        async fn attested_domain_must_be_a_hostname() {
            let s = tls_server("x").await;
            for d in ["", "a/b", "a b", "evil.test@x", "-a.test", "a.test.", "h:1"] {
                let t = ProxyTarget {
                    domain: d.into(),
                    spki_sha256_hex: s.leaf_spki_hex.clone(),
                };
                assert_eq!(
                    fetch_proxy_pinned(&cfg_for(&s), &t).await.unwrap_err(),
                    "attested domain is not a hostname",
                    "{d:?}"
                );
            }
            assert_eq!(s.requests.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn attested_domain_must_be_a_tinfoil_router_host() {
            let s = tls_server("x").await;
            for d in [
                "evil.example.com",
                "tinfoil.sh",
                "inference.tinfoil.sh.evil.com",
                "eviltinfoil.sh",
                "Inference.tinfoil.sh",
                "xn--a.tinfoil.sh",
                "a..tinfoil.sh",
                "localhost",
                "127.0.0.1",
            ] {
                let t = ProxyTarget {
                    domain: d.into(),
                    spki_sha256_hex: s.leaf_spki_hex.clone(),
                };
                assert_eq!(
                    fetch_proxy_pinned(&cfg_for(&s), &t).await.unwrap_err(),
                    "attested domain is not a hostname",
                    "{d:?}"
                );
            }
            assert_eq!(s.requests.load(Ordering::SeqCst), 0);
            assert!(validate_router_domain("inference.tinfoil.sh").is_ok());
        }

        #[tokio::test]
        async fn probe_does_not_trust_a_proxy_document_from_an_unattested_key() {
            // The captured ATC bundle attests a TLS key this server does not
            // hold, so the document it serves must not become observations.
            let (_s, mut cfg) = server().await;
            let tls = tls_server(PROXY_LEAKED).await;
            cfg.proxy_fetcher = None;
            cfg.proxy_net = ProxyNet {
                roots: Some(tls.roots.clone()),
                connect_to: Some(tls.addr),
            };
            let out = run(&reqwest::Client::new(), &cfg).await.unwrap();
            assert!(out.observations.router.is_some(), "router still observed");
            assert!(out.observations.models.is_empty());
            assert!(out.sigstore.models.is_empty());
            assert!(out.notes.iter().any(|n| n.starts_with("proxy document:")));
            assert_eq!(tls.requests.load(Ordering::SeqCst), 0);
        }

        const PROXY_LEAKED: &str = include_str!("../testdata/proxy.json");
    }

    #[tokio::test]
    async fn proxy_is_not_fetched_unless_the_router_verifies() {
        let (s, mut cfg) = server().await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let inner = cfg.proxy_fetcher.take().unwrap();
        cfg.proxy_fetcher = Some(Arc::new(move |t| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            inner(t)
        }));
        let mut atc: serde_json::Value = serde_json::from_str(ATC).unwrap();
        atc["enclaveAttestationReport"]["format"] =
            serde_json::json!("https://tinfoil.sh/predicate/tdx-guest/v2");
        Mock::given(method("GET"))
            .and(path("/atc-unverified"))
            .respond_with(ResponseTemplate::new(200).set_body_string(atc.to_string()))
            .mount(&s)
            .await;
        cfg.atc_url = format!("{}/atc-unverified", s.uri());
        let out = run(&reqwest::Client::new(), &cfg).await.unwrap();
        assert!(out.observations.router.is_none());
        assert!(out.observations.models.is_empty());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(out
            .notes
            .iter()
            .any(|n| n.contains("proxy document: not fetched")));
    }

    #[tokio::test]
    async fn fetcher_receives_the_verified_domain_and_attested_key() {
        let (_s, mut cfg) = server().await;
        let got = Arc::new(std::sync::Mutex::new(None));
        let slot = got.clone();
        cfg.proxy_fetcher = Some(Arc::new(move |t| {
            *slot.lock().unwrap() = Some(t);
            Box::pin(async { Err("stop".to_string()) })
        }));
        let out = run(&reqwest::Client::new(), &cfg).await.unwrap();
        let t = got.lock().unwrap().clone().expect("fetcher called");
        assert_eq!(t.domain, "inference.tinfoil.sh");
        assert_eq!(
            t.spki_sha256_hex,
            out.observations.router.unwrap().spki_sha256_hex
        );
        assert!(out.notes.iter().any(|n| n == "proxy document: stop"));
    }

    /// A valid proxy document padded to `n` distinct releases (cloned
    /// first model, unique tags).
    fn proxy_with_releases(n: usize) -> String {
        let mut doc: serde_json::Value = serde_json::from_str(PROXY).unwrap();
        let models = doc["models"].as_object_mut().unwrap();
        let template = models.values().next().unwrap().clone();
        models.clear();
        for i in 0..n {
            let mut m = template.clone();
            m["tag"] = serde_json::json!(format!("v9.9.{i}"));
            models.insert(format!("m{i}"), m);
        }
        doc.to_string()
    }

    async fn run_with_proxy_body(body: String) -> (MockServer, ProbeOutput) {
        let (s, mut cfg) = server().await;
        Mock::given(method("GET"))
            .and(path("/proxy-padded"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&s)
            .await;
        cfg.proxy_fetcher = Some(mock_proxy_fetcher(format!("{}/proxy-padded", s.uri())));
        let out = run(&reqwest::Client::new(), &cfg).await.unwrap();
        (s, out)
    }

    async fn release_requests(s: &MockServer) -> usize {
        s.received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() != "/atc" && !r.url.path().starts_with("/proxy"))
            .count()
    }

    #[tokio::test]
    async fn proxy_document_over_release_limit_makes_no_release_fetches() {
        let (s, out) = run_with_proxy_body(proxy_with_releases(MAX_RELEASES + 1)).await;
        assert!(!out.complete);
        assert!(out.observations.models.is_empty());
        assert!(out.sigstore.models.is_empty());
        assert_eq!(release_requests(&s).await, 0);
        assert!(out
            .notes
            .iter()
            .any(|n| n.starts_with("proxy document:") && n.contains("exceeds limit")));
    }

    #[tokio::test]
    async fn proxy_document_at_release_limit_is_still_processed() {
        let (s, out) = run_with_proxy_body(proxy_with_releases(MAX_RELEASES)).await;
        assert!(out.complete);
        assert_eq!(out.observations.models.len(), MAX_RELEASES);
        // Each release is attempted (none has a mocked hash, so each is noted).
        assert!(release_requests(&s).await >= MAX_RELEASES);
    }
}
