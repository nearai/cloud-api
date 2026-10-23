//! Daily Tinfoil measurement sync, run by
//! `.github/workflows/chutes-measurements-sync.yml`.
//!
//! The probe ([`probe`]) reads Tinfoil's public endpoints (no API key): the
//! router's ATC attestation bundle and `/.well-known/tinfoil-proxy`. The router
//! is observed with the same SEV-SNP checks the serving verifier uses. Each
//! release is then checked against its Sigstore attestation
//! ([`tinfoil_provenance`]). The classifier ([`classify`]) adds a row to the
//! compiled pins file (`services::attestation::tinfoil_pins`) only if the row
//! was seen live and equals the measurement in the Sigstore predicate. Rows are
//! never removed. A person reviews and merges the resulting PR. This crate is
//! CI tooling, never a dependency of `api` or `services`, and is not part of
//! the API image. The runtime verifies the ATC `sigstore_bundle` through
//! `tinfoil_provenance`, the only crate allowed to depend on `sigstore-verify`
//! (enforced by `scripts/check_dep_guards.sh`).

pub mod classify;
pub mod cli;
pub mod probe;
pub mod report;

use classify::{Earlier, Observations};
use services::attestation::tinfoil_pins::TinfoilPins;
use services::attestation::tinfoil_trust::MAX_MODEL_PINS;

/// Rows that earlier runs verified: the `verified` field of every
/// `observations.json` audit file below `dir` (the downloaded artifacts of
/// recent successful runs on `main`), each with the time that run observed it.
/// Files that do not parse are skipped.
///
/// The directory walk is a deliberate fork of `load_evidence` in
/// `crates/chutes_sync/src/lib.rs` (spec section 3.4): the two syncs share no
/// code so neither can break the other.
pub fn load_evidence(dir: &std::path::Path) -> Earlier {
    #[derive(serde::Deserialize)]
    struct Audit {
        verified: TinfoilPins,
        #[serde(default)]
        observations: Option<AuditObservations>,
        #[serde(default)]
        date: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct AuditObservations {
        observed_at: String,
    }
    let mut out = Earlier::default();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == "observations.json") {
                match std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<Audit>(&s).ok())
                {
                    Some(a) => {
                        let at = a
                            .observations
                            .map(|o| o.observed_at)
                            .or(a.date)
                            .unwrap_or_else(|| "unknown".to_string());
                        out.merge(&a.verified, &at)
                    }
                    None => eprintln!(
                        "tinfoil-measurement-sync: skipping unreadable evidence {}",
                        path.display()
                    ),
                }
            }
        }
    }
    out
}

/// The audit file written next to the report.
pub fn audit_json(
    date: &str,
    observations: &Observations,
    sigstore: &classify::SigstoreResults,
    verified: &TinfoilPins,
    notes: &[String],
) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "date": date,
        "observations": observations,
        "sigstore": sigstore,
        "verified": verified,
        "notes": notes,
    }))
    .expect("audit serialises")
}

/// Bytes of `model_pins` rows one artifact may carry: half of the setting's
/// size limit, leaving room for the pins already stored, since the PATCH
/// replaces the whole list.
pub const ADMIN_PINS_BYTE_BUDGET: usize = services::attestation::tinfoil_trust::TRUST_MAX_BYTES / 2;

/// Model rows one artifact may carry: half of the setting's pin limit, so the
/// merged PATCH keeps room for pins already stored, like the byte budget.
pub const ADMIN_PINS_MAX_ROWS: usize = MAX_MODEL_PINS / 2;

/// The rows of the `PATCH /v1/admin/settings/tinfoil_trust` body that trusts
/// the model releases seen live and Sigstore-verified but not yet in the
/// compiled pins, each with the release's raw Sigstore bundle. The rows stay
/// within what one PATCH accepts (at most [`ADMIN_PINS_MAX_ROWS`] rows and
/// [`ADMIN_PINS_BYTE_BUDGET`] bytes); the second value names the candidates
/// left out (`slug repo@tag`, identifiers only) so they can be surfaced.
pub fn admin_pin_rows(
    base: &TinfoilPins,
    observations: &Observations,
    sigstore: &classify::SigstoreResults,
    bundles: &std::collections::BTreeMap<String, serde_json::Value>,
) -> (Vec<serde_json::Value>, Vec<String>) {
    let (mut rows, mut left_out, mut bytes) = (Vec::new(), Vec::new(), 0usize);
    for (slug, repo, tag) in classify::admin_pin_candidates(base, observations, sigstore) {
        let Some(bundle) = bundles.get(&format!("{repo}@{tag}")) else {
            continue;
        };
        let row = serde_json::json!({
            "slug": slug,
            "repo": repo,
            "tag": tag,
            "sigstore_bundle": bundle,
        });
        let size = row.to_string().len();
        if rows.len() >= ADMIN_PINS_MAX_ROWS || bytes + size > ADMIN_PINS_BYTE_BUDGET {
            left_out.push(format!("{slug} {repo}@{tag}"));
        } else {
            bytes += size;
            rows.push(row);
        }
    }
    (rows, left_out)
}

/// The artifact body: `{"model_pins":[...]}`, empty when nothing is new.
pub fn admin_pins_json(rows: &[serde_json::Value]) -> String {
    serde_json::to_string_pretty(&serde_json::json!({ "model_pins": rows }))
        .expect("admin pins serialise")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` live, verified, uncompiled model releases with a bundle each.
    fn candidates(
        n: usize,
        bundle_len: usize,
    ) -> (
        Observations,
        classify::SigstoreResults,
        std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        let regs = ["aa", "r1", "r2"];
        let mut observed = Observations {
            observed_at: "t".into(),
            router: None,
            models: vec![],
        };
        let mut sigstore = classify::SigstoreResults::default();
        let mut bundles = std::collections::BTreeMap::new();
        for i in 0..n {
            let (slug, repo) = (format!("m{i:03}"), format!("tinfoilsh/m{i:03}"));
            observed.models.push(classify::ModelObservation {
                slug: slug.clone(),
                repo: repo.clone(),
                tag: "v1".into(),
                kind: "p".into(),
                registers: regs.iter().map(|r| r.to_string()).collect(),
            });
            sigstore.models.insert(
                format!("{repo}@v1"),
                tinfoil_provenance::Provenance {
                    repo: repo.clone(),
                    tag: "v1".into(),
                    predicate_type: "p".into(),
                    subject_sha256: "d".into(),
                    snp_measurement: "aa".into(),
                    rtmr1: Some("r1".into()),
                    rtmr2: Some("r2".into()),
                    rekor_log_index: 1,
                    integrated_time: chrono::DateTime::UNIX_EPOCH,
                },
            );
            bundles.insert(
                format!("{repo}@v1"),
                serde_json::json!({ "pad": "x".repeat(bundle_len) }),
            );
        }
        (observed, sigstore, bundles)
    }

    #[test]
    fn admin_pin_rows_stop_at_the_pin_count_limit_and_name_the_rest() {
        // The cap is now ADMIN_PINS_MAX_ROWS (half the setting limit), so the
        // left-out names start at that index instead of MAX_MODEL_PINS.
        let n = ADMIN_PINS_MAX_ROWS;
        let (o, s, b) = candidates(n + 3, 10);
        let (rows, left_out) = admin_pin_rows(&TinfoilPins::default(), &o, &s, &b);
        assert_eq!(rows.len(), n);
        assert_eq!(
            left_out,
            (n..n + 3)
                .map(|i| format!("m{i:03} tinfoilsh/m{i:03}@v1"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn admin_pin_rows_stop_at_the_byte_budget() {
        let (o, s, b) = candidates(8, ADMIN_PINS_BYTE_BUDGET / 5);
        let (rows, left_out) = admin_pin_rows(&TinfoilPins::default(), &o, &s, &b);
        assert_eq!(rows.len(), 4);
        assert_eq!(left_out.len(), 4);
        let body = admin_pins_json(&rows);
        assert!(body.len() <= services::attestation::tinfoil_trust::TRUST_MAX_BYTES);
    }

    #[test]
    fn evidence_is_merged_from_every_observations_file_below_the_dir() {
        let dir = std::env::temp_dir().join(format!("tinfoil-evidence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run1/artifact")).unwrap();
        std::fs::create_dir_all(dir.join("run2")).unwrap();
        let row = |m: &str| serde_json::json!({"router":[{"measurement":m,"repo":"r","tag":"t"}],"models":{}});
        for (p, m) in [("run1/artifact", "aa"), ("run2", "bb")] {
            std::fs::write(
                dir.join(p).join("observations.json"),
                serde_json::json!({"verified": row(m)}).to_string(),
            )
            .unwrap();
        }
        std::fs::write(dir.join("run2/report.md"), "# ignored").unwrap();
        std::fs::create_dir_all(dir.join("run3")).unwrap();
        std::fs::write(dir.join("run3/observations.json"), "not json").unwrap();
        let loaded = load_evidence(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        let ms: Vec<_> = loaded
            .pins
            .router
            .iter()
            .map(|r| r.measurement.as_str())
            .collect();
        assert_eq!(ms, vec!["aa", "bb"]);
    }
}
