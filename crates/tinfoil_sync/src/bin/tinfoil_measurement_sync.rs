//! Daily Tinfoil measurement sync (run by
//! `.github/workflows/tinfoil-measurements-sync.yml`).
//!
//! Usage: `tinfoil_measurement_sync --pins <path> --out-dir <dir> [--evidence-dir <dir>]`
//!
//! Reads and rewrites the pins file in place (canonical JSON) and writes
//! `<out-dir>/report.md` (the PR body) and `<out-dir>/observations.json` (the
//! audit artifact). No Tinfoil key is used; the optional `GITHUB_TOKEN` only
//! raises GitHub's rate limit for public attestation reads. Exit codes: 0 ok
//! (changed or not), 1 probe failed, 2 bad input. Logs counts only.

use std::path::PathBuf;
use std::process::ExitCode;

use services::attestation::tinfoil_pins::TinfoilPins;
use tinfoil_sync::classify::classify_with_earlier;
use tinfoil_sync::probe::{self, ProbeConfig};
use tinfoil_sync::report::render_markdown;
use tinfoil_sync::{audit_json, load_evidence};

type Failure = (u8, String);

fn bad(msg: String) -> Failure {
    (2, msg)
}

struct Args {
    pins: PathBuf,
    out_dir: PathBuf,
    evidence_dir: Option<PathBuf>,
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, Failure> {
    let (mut pins, mut out_dir, mut evidence_dir) = (None, None, None);
    while let Some(flag) = it.next() {
        let slot = match flag.as_str() {
            "--pins" => &mut pins,
            "--out-dir" => &mut out_dir,
            "--evidence-dir" => &mut evidence_dir,
            other => return Err(bad(format!("unknown argument {other}"))),
        };
        *slot = Some(PathBuf::from(
            it.next()
                .ok_or_else(|| bad(format!("{flag} needs a value")))?,
        ));
    }
    Ok(Args {
        pins: pins.ok_or_else(|| bad("--pins is required".into()))?,
        out_dir: out_dir.ok_or_else(|| bad("--out-dir is required".into()))?,
        evidence_dir,
    })
}

fn write(path: &std::path::Path, contents: &str) -> Result<(), Failure> {
    std::fs::write(path, contents).map_err(|e| bad(format!("write {}: {e}", path.display())))
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, msg)) => {
            eprintln!("tinfoil-measurement-sync: {msg}");
            ExitCode::from(code)
        }
    }
}

async fn run() -> Result<(), Failure> {
    let args = parse_args(std::env::args().skip(1))?;
    let base: TinfoilPins = serde_json::from_str(
        &std::fs::read_to_string(&args.pins)
            .map_err(|e| bad(format!("read {}: {e}", args.pins.display())))?,
    )
    .map_err(|e| bad(format!("parse {}: {e}", args.pins.display())))?;
    std::fs::create_dir_all(&args.out_dir)
        .map_err(|e| bad(format!("create {}: {e}", args.out_dir.display())))?;
    let earlier = args
        .evidence_dir
        .as_deref()
        .map(load_evidence)
        .unwrap_or_default();

    let cfg = ProbeConfig {
        github_token: std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()),
        ..ProbeConfig::default()
    };
    let out = probe::run(&reqwest::Client::new(), &cfg)
        .await
        .map_err(|e| (1, e.to_string()))?;

    let (pins, added) = classify_with_earlier(&base, &earlier, &out.observations, &out.sigstore);
    let (verified, _) =
        tinfoil_sync::classify::classify(&TinfoilPins::default(), &out.observations, &out.sigstore);
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    write(&args.pins, &pins.to_canonical_json())?;
    write(
        &args.out_dir.join("report.md"),
        &render_markdown(&added, &out.notes, &date),
    )?;
    write(
        &args.out_dir.join("observations.json"),
        &audit_json(
            &date,
            &out.observations,
            &out.sigstore,
            &verified,
            &out.notes,
        ),
    )?;
    eprintln!(
        "tinfoil-measurement-sync: router_observed={} models_published={} releases_verified={} not_pinned={} rows_added={}",
        out.observations.router.is_some(),
        out.observations.models.len(),
        out.sigstore.models.len() + usize::from(out.sigstore.router.is_some()),
        out.notes.len(),
        added.len(),
    );
    Ok(())
}
