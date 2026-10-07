//! Daily Chutes measurement sync (run by
//! `.github/workflows/chutes-measurements-sync.yml`).
//!
//! Env:
//! No Chutes API key is used: the probe reads Chutes' public `/v1/models`,
//! `/servers/tee/measurements` and `/chutes/{id}/evidence` endpoints.
//!
//! - `PCCS_URL` (optional): DCAP collateral server.
//! - `CHUTES_SYNC_MODELS` (optional): comma-separated model ids; when set,
//!   only these models are probed.
//! - `PINS_BASE` (required): main's pins file.
//! - `PINS_EVIDENCE_DIR` (optional): downloaded artifacts of recent
//!   successful runs on `main`; rows their `observations.json` files show as
//!   verified are pinned while Chutes still publishes them.
//! - `PINS_OUT` (required): where to write the merged pins file.
//! - `REPORT_OUT` (required): markdown report path.
//! - `OBSERVATIONS_OUT` (required): JSON audit file (observations, skipped
//!   chutes, raw quotes).
//!
//! Exit codes: 0 ok, 1 probe failed (feed or model list unreachable, or every
//! chute failed), 2 bad input. Logs IDs and counts only.

use std::collections::BTreeSet;
use std::process::ExitCode;

use chutes_sync::classify::classify;
use chutes_sync::probe::{self, ProbeConfig};
use chutes_sync::report::render_markdown;
use chutes_sync::{load_evidence, parse_model_list};
use inference_providers::attested::chutes::client::ChutesClient;
use services::attestation::chutes::ChutesObserver;
use services::attestation::chutes_pins::PinsFile;

/// Per-request timeout for model-list and evidence calls, in seconds.
const CHUTES_TIMEOUT_SECS: u64 = 60;

type Failure = (u8, String);

fn bad(msg: String) -> Failure {
    (2, msg)
}

fn required(name: &str) -> Result<String, Failure> {
    optional(name).ok_or_else(|| bad(format!("{name} is required")))
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

fn read_pins(path: &str) -> Result<PinsFile, Failure> {
    let s = std::fs::read_to_string(path).map_err(|e| bad(format!("read {path}: {e}")))?;
    PinsFile::parse(&s).map_err(|e| bad(format!("parse {path}: {e}")))
}

fn write(path: &str, contents: &str) -> Result<(), Failure> {
    std::fs::write(path, contents).map_err(|e| bad(format!("write {path}: {e}")))
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, msg)) => {
            eprintln!("chutes-measurement-sync: {msg}");
            ExitCode::from(code)
        }
    }
}

async fn run() -> Result<(), Failure> {
    let base = read_pins(&required("PINS_BASE")?)?;
    let out_path = required("PINS_OUT")?;
    let report_path = required("REPORT_OUT")?;
    let obs_path = required("OBSERVATIONS_OUT")?;
    let earlier = optional("PINS_EVIDENCE_DIR")
        .map(|d| load_evidence(std::path::Path::new(&d)))
        .unwrap_or_default();
    let only_models = optional("CHUTES_SYNC_MODELS").map(|s| parse_model_list(&s));

    let client = ChutesClient::public(CHUTES_TIMEOUT_SECS)
        .map_err(|e| bad(format!("Chutes client: {e}")))?;
    let observer = ChutesObserver::new(optional("PCCS_URL"));
    let cfg = ProbeConfig {
        only_models,
        ..ProbeConfig::default()
    };
    let out = probe::run(&client, &reqwest::Client::new(), &observer, &cfg)
        .await
        .map_err(|e| (1, e.to_string()))?;

    let (pins, report) = classify(
        &out.feed,
        &out.observations,
        &earlier,
        out.skipped.clone(),
        &base,
    );
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    write(&out_path, &pins.to_canonical_json())?;
    write(&report_path, &render_markdown(&report, &date))?;
    let audit = serde_json::json!({
        "date": date,
        "observations": out.observations,
        "skipped": out.skipped,
        "quotes": out.quotes,
    });
    write(
        &obs_path,
        &serde_json::to_string_pretty(&audit).map_err(|e| bad(format!("audit json: {e}")))?,
    )?;

    let chutes: BTreeSet<_> = out.observations.iter().map(|o| &o.chute_id).collect();
    eprintln!(
        "chutes-measurement-sync: chutes_with_instances={} instances={} skipped_chutes={} rows_added={}",
        chutes.len(),
        out.observations.len(),
        out.skipped.len(),
        report.added.len(),
    );
    Ok(())
}
