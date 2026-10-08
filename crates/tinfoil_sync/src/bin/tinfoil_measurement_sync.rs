//! Daily Tinfoil measurement sync (run by
//! `.github/workflows/chutes-measurements-sync.yml`).
//!
//! Usage: `tinfoil_measurement_sync --pins <path> --out-dir <dir> [--evidence-dir <dir>]`
//!
//! Reads and rewrites the pins file in place (canonical JSON) and writes
//! `<out-dir>/report.md` (the PR body) and `<out-dir>/observations.json` (the
//! audit artifact). No Tinfoil key is used; the optional `GITHUB_TOKEN` only
//! raises GitHub's rate limit for public attestation reads. Exit codes: 0 ok
//! (changed or not), 1 probe failed, 2 bad input. Logs counts only.

use std::process::ExitCode;

use tinfoil_sync::cli::{parse_args, run_sync, Failure};
use tinfoil_sync::probe::ProbeConfig;

async fn run() -> Result<(), Failure> {
    let args = parse_args(std::env::args().skip(1))?;
    let cfg = ProbeConfig {
        github_token: std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()),
        ..ProbeConfig::default()
    };
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| (2, format!("http client: {e}")))?;
    let summary = run_sync(&args, &client, &cfg).await?;
    eprintln!("tinfoil-measurement-sync: {summary}");
    Ok(())
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
