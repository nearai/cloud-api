//! Argument parsing and the sync core, split from the binary so it can be
//! tested against a mock probe.

use std::path::{Path, PathBuf};

use services::attestation::tinfoil_pins::TinfoilPins;

use crate::classify::{classify_with_earlier, rejections};
use crate::probe::{self, ProbeConfig};
use crate::report::render_markdown;
use crate::{audit_json, load_evidence};

/// Exit code and message.
pub type Failure = (u8, String);

fn bad(msg: String) -> Failure {
    (2, msg)
}

#[derive(Debug)]
pub struct Args {
    pub pins: PathBuf,
    pub out_dir: PathBuf,
    pub evidence_dir: Option<PathBuf>,
}

pub fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, Failure> {
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

fn write(path: &Path, contents: &str) -> Result<(), Failure> {
    std::fs::write(path, contents).map_err(|e| bad(format!("write {}: {e}", path.display())))
}

/// Runs one sync: probe, classify, rewrite the pins file (only if rows were
/// added) and write `report.md` and `observations.json`. Returns a counts-only
/// summary line.
pub async fn run_sync(
    args: &Args,
    client: &reqwest::Client,
    cfg: &ProbeConfig,
) -> Result<String, Failure> {
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

    let out = probe::run(client, cfg)
        .await
        .map_err(|e| (1, e.to_string()))?;

    let (pins, added) = classify_with_earlier(&base, &earlier, &out.observations, &out.sigstore);
    // Disagreements between a live row and its verified Sigstore predicate are
    // reported with the other not-pinned notes.
    let mut notes = out.notes.clone();
    notes.extend(rejections(&out.observations, &out.sigstore));
    let (verified, _) =
        crate::classify::classify(&TinfoilPins::default(), &out.observations, &out.sigstore);
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    // Leave the file untouched when nothing was added, so an unchanged run
    // produces no diff (and so no PR).
    if pins != base {
        write(&args.pins, &pins.to_canonical_json())?;
    }
    write(
        &args.out_dir.join("report.md"),
        &render_markdown(&added, &notes, &date),
    )?;
    write(
        &args.out_dir.join("observations.json"),
        &audit_json(&date, &out.observations, &out.sigstore, &verified, &notes),
    )?;
    Ok(format!(
        "router_observed={} models_published={} releases_verified={} not_pinned={} rows_added={}",
        out.observations.router.is_some(),
        out.observations.models.len(),
        out.sigstore.models.len() + usize::from(out.sigstore.router.is_some()),
        notes.len(),
        added.len(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Args, Failure> {
        parse_args(v.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parse_args_accepts_required_and_optional_flags() {
        let a = args(&["--pins", "p.json", "--out-dir", "o", "--evidence-dir", "e"]).unwrap();
        assert_eq!(a.pins, PathBuf::from("p.json"));
        assert_eq!(a.out_dir, PathBuf::from("o"));
        assert_eq!(a.evidence_dir, Some(PathBuf::from("e")));
        assert!(args(&["--pins", "p", "--out-dir", "o"])
            .unwrap()
            .evidence_dir
            .is_none());
    }

    #[test]
    fn parse_args_rejects_unknown_and_missing() {
        assert_eq!(args(&["--bogus", "x"]).unwrap_err().0, 2);
        assert!(args(&["--pins"]).unwrap_err().1.contains("needs a value"));
        assert!(args(&["--out-dir", "o"])
            .unwrap_err()
            .1
            .contains("--pins is required"));
        assert!(args(&["--pins", "p"])
            .unwrap_err()
            .1
            .contains("--out-dir is required"));
    }
}
