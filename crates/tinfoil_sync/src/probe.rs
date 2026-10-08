//! Network side of the sync: fetch the router's ATC bundle and the proxy
//! document, observe the router, and collect Sigstore attestations for every
//! release in play.
//!
//! Only public endpoints are used and no Tinfoil key. Errors are reported by
//! HTTP status or category only, never with the upstream body.

use std::collections::BTreeMap;
use std::time::Duration;

use inference_providers::attested::tinfoil::verifier_port::{AtcBundle, ProxyDoc};
use services::attestation::tinfoil_observer::observe;

use crate::classify::{
    ModelObservation, Observations, RouterObservation, SigstoreResults, ROUTER_REPO,
};
use crate::sigstore_verify::verify_bundle;

pub const ATC_URL: &str = "https://atc.tinfoil.sh/attestation";
pub const PROXY_URL: &str = "https://inference.tinfoil.sh/.well-known/tinfoil-proxy";
pub const GITHUB_DOWNLOAD_BASE: &str = "https://github.com";
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// Largest response body accepted from any endpoint (the ATC bundle and proxy
/// document are well under 1 MiB).
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;

pub struct ProbeConfig {
    pub atc_url: String,
    pub proxy_url: String,
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
            proxy_url: PROXY_URL.into(),
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

pub async fn run(client: &reqwest::Client, cfg: &ProbeConfig) -> Result<ProbeOutput, ProbeError> {
    let mut notes = Vec::new();
    let atc_text = get_text(client, cfg, &cfg.atc_url, None)
        .await
        .map_err(|e| ProbeError::Fetch(format!("ATC bundle: {e}")))?;
    let bundle: AtcBundle = serde_json::from_str(&atc_text)
        .map_err(|_| ProbeError::Fetch("ATC bundle: malformed".into()))?;
    let proxy_text = get_text(client, cfg, &cfg.proxy_url, None)
        .await
        .map_err(|e| ProbeError::Fetch(format!("proxy document: {e}")))?;
    let proxy: ProxyDoc = serde_json::from_str(&proxy_text)
        .map_err(|_| ProbeError::Fetch("proxy document: malformed".into()))?;

    let observed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut observations = Observations {
        observed_at,
        ..Observations::default()
    };
    let mut sigstore = SigstoreResults::default();

    match observe(&bundle) {
        Ok(o) => {
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
            proxy_url: format!("{}/proxy", s.uri()),
            download_base: s.uri(),
            api_base: s.uri(),
            github_token: None,
            attempts: 1,
            backoff: Duration::ZERO,
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
            proxy_url: format!("{}/proxy", s.uri()),
            download_base: s.uri(),
            api_base: s.uri(),
            github_token: None,
            attempts,
            backoff: Duration::ZERO,
        }
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
}
