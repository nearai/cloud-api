//! Runs the sync core against a mock probe and a temp pins file.

use std::sync::Arc;
use std::time::Duration;

use services::attestation::tinfoil_pins::TinfoilPins;
use tinfoil_sync::cli::{run_sync, Args};
use tinfoil_sync::load_evidence;
use tinfoil_sync::probe::ProbeConfig;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ATC: &str = include_str!("../testdata/atc_attestation.out");
const PROXY: &str = include_str!("../testdata/proxy.json");
const MODEL_BUNDLE: &str = include_str!("../testdata/model_sigstore_bundle.json");
const DIGEST: &str = "8448fed68f4ed10c829a6433bdf570fa12a7b0e6d589bf1a89a8e9a5ca37a3ac";
const REPO: &str = "tinfoilsh/confidential-deepseek-v4-1-flash";

async fn server() -> (MockServer, ProbeConfig) {
    let s = MockServer::start().await;
    for (p, body) in [("/atc", ATC.to_string()), ("/proxy", PROXY.to_string())] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&s)
            .await;
    }
    Mock::given(method("GET"))
        .and(path(format!(
            "/{REPO}/releases/download/v0.0.3/tinfoil.hash"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(DIGEST))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/attestations/sha256:{DIGEST}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"attestations":[{{"bundle":{MODEL_BUNDLE}}}]}}"#
        )))
        .mount(&s)
        .await;
    // The pinned proxy fetch is covered in `probe.rs` against a local TLS
    // server; here a plain mock stands in for it so the rest of the sync runs.
    let proxy_url = format!("{}/proxy", s.uri());
    let cfg = ProbeConfig {
        atc_url: format!("{}/atc", s.uri()),
        proxy_fetcher: Some(Arc::new(move |_target| {
            let url = proxy_url.clone();
            Box::pin(async move {
                reqwest::get(&url)
                    .await
                    .map_err(|e| e.to_string())?
                    .text()
                    .await
                    .map_err(|e| e.to_string())
            })
        })),
        download_base: s.uri(),
        api_base: s.uri(),
        attempts: 1,
        backoff: Duration::ZERO,
        ..ProbeConfig::default()
    };
    (s, cfg)
}

#[tokio::test]
async fn sync_writes_canonical_pins_and_round_trippable_evidence() {
    let (_s, cfg) = server().await;
    let dir = std::env::temp_dir().join(format!("tinfoil-sync-run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pins_path = dir.join("pins.json");
    std::fs::write(&pins_path, TinfoilPins::default().to_canonical_json()).unwrap();
    let args = Args {
        pins: pins_path.clone(),
        out_dir: dir.join("out"),
        evidence_dir: None,
    };
    let client = reqwest::Client::new();

    let summary = run_sync(&args, &client, &cfg).await.unwrap();
    assert!(summary.contains("router_observed=true"), "{summary}");

    let written = std::fs::read_to_string(&pins_path).unwrap();
    let pins: TinfoilPins = serde_json::from_str(&written).unwrap();
    assert_eq!(written, pins.to_canonical_json(), "pins file is canonical");
    assert_eq!(pins.router.len(), 1);
    assert!(!pins.models.is_empty());

    // The audit file's `verified` rows round-trip through load_evidence.
    assert!(dir.join("out/report.md").is_file());
    assert_eq!(load_evidence(&dir.join("out")).pins, pins);

    // A second run adds nothing and leaves the pins file byte-identical.
    let second = run_sync(&args, &client, &cfg).await.unwrap();
    assert!(second.contains("rows_added=0"), "{second}");
    assert_eq!(std::fs::read_to_string(&pins_path).unwrap(), written);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn unchanged_pins_file_is_not_rewritten() {
    let (_s, cfg) = server().await;
    let dir = std::env::temp_dir().join(format!("tinfoil-sync-noop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Seed with an empty file, sync once to reach the full set, then store the
    // same rows in a non-canonical layout: a no-change run must not rewrite it.
    let pins_path = dir.join("pins.json");
    std::fs::write(&pins_path, TinfoilPins::default().to_canonical_json()).unwrap();
    let args = Args {
        pins: pins_path.clone(),
        out_dir: dir.join("out"),
        evidence_dir: None,
    };
    let client = reqwest::Client::new();
    run_sync(&args, &client, &cfg).await.unwrap();
    let full: TinfoilPins =
        serde_json::from_str(&std::fs::read_to_string(&pins_path).unwrap()).unwrap();
    let odd = serde_json::to_string(&full).unwrap(); // single line, not canonical
    std::fs::write(&pins_path, &odd).unwrap();
    run_sync(&args, &client, &cfg).await.unwrap();
    assert_eq!(std::fs::read_to_string(&pins_path).unwrap(), odd);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Seeds a pins file with a full set of rows in a non-canonical layout, then
/// runs a sync against `cfg` that must fail.
async fn failing_run_keeps_pins_and_writes_evidence(cfg: &ProbeConfig, tag: &str) {
    let (_s, good) = server().await;
    let dir = std::env::temp_dir().join(format!("tinfoil-sync-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pins_path = dir.join("pins.json");
    std::fs::write(&pins_path, TinfoilPins::default().to_canonical_json()).unwrap();
    let args = Args {
        pins: pins_path.clone(),
        out_dir: dir.join("out"),
        evidence_dir: None,
    };
    let client = reqwest::Client::new();
    run_sync(&args, &client, &good).await.unwrap();
    let full: TinfoilPins =
        serde_json::from_str(&std::fs::read_to_string(&pins_path).unwrap()).unwrap();
    let odd = serde_json::to_string(&full).unwrap();
    std::fs::write(&pins_path, &odd).unwrap();
    std::fs::remove_dir_all(dir.join("out")).unwrap();

    let (code, msg) = run_sync(&args, &client, cfg).await.unwrap_err();
    assert_eq!(code, 1, "{msg}");
    assert_eq!(std::fs::read_to_string(&pins_path).unwrap(), odd);
    assert!(dir.join("out/report.md").is_file());
    assert!(dir.join("out/observations.json").is_file());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn unverified_router_fails_the_run_after_writing_evidence() {
    let (s, mut cfg) = server().await;
    let mut atc: serde_json::Value = serde_json::from_str(ATC).unwrap();
    atc["enclaveAttestationReport"]["format"] =
        serde_json::json!("https://tinfoil.sh/predicate/tdx-guest/v2");
    Mock::given(method("GET"))
        .and(path("/atc-unverified"))
        .respond_with(ResponseTemplate::new(200).set_body_string(atc.to_string()))
        .mount(&s)
        .await;
    cfg.atc_url = format!("{}/atc-unverified", s.uri());
    failing_run_keeps_pins_and_writes_evidence(&cfg, "router").await;
}

#[tokio::test]
async fn unfetched_proxy_document_fails_the_run_after_writing_evidence() {
    let (_s, mut cfg) = server().await;
    cfg.proxy_fetcher = Some(Arc::new(|_t| Box::pin(async { Err("down".to_string()) })));
    failing_run_keeps_pins_and_writes_evidence(&cfg, "proxy").await;
}

#[tokio::test]
async fn malformed_proxy_document_fails_the_run() {
    let (_s, mut cfg) = server().await;
    cfg.proxy_fetcher = Some(Arc::new(|_t| Box::pin(async { Ok("{".to_string()) })));
    failing_run_keeps_pins_and_writes_evidence(&cfg, "malformed").await;
}
