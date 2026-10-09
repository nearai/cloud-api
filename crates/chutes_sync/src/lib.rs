//! Daily Chutes measurement sync, run by
//! `.github/workflows/chutes-measurements-sync.yml`.
//!
//! The probe ([`probe`]) asks every Chutes TEE chute for fresh evidence
//! through Chutes' public endpoints (no API key) and records the registers of
//! each instance whose quote passes the Intel signature chain, TCB floor and
//! debug-bit check, is bound to our nonce (the instance certificate the quote
//! binds signed the evidence body carrying it), and whose GPUs pass NVIDIA
//! NRAS. The classifier ([`classify`]) then adds a row to the compiled
//! pins file (`services::attestation::chutes_pins`) only if all five registers
//! equal a row Chutes publishes. Rows are never removed, and a row whose
//! runtime RTMR3 is all zeros is never added. A person reviews and merges the
//! resulting PR; this crate is tooling and is not part of the API image.

pub mod classify;
pub mod probe;
pub mod report;

/// Observations recorded by earlier runs of this job: every
/// `observations.json` audit file below `dir` (the downloaded artifacts of
/// recent successful runs on `main`). Files that do not parse are skipped.
pub fn load_evidence(dir: &std::path::Path) -> Vec<classify::Observation> {
    #[derive(serde::Deserialize)]
    struct Audit {
        observations: Vec<classify::Observation>,
    }
    let mut out = Vec::new();
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
                    Some(a) => out.extend(a.observations),
                    None => eprintln!(
                        "chutes-measurement-sync: skipping unreadable evidence {}",
                        path.display()
                    ),
                }
            }
        }
    }
    out
}

/// Parse `CHUTES_SYNC_MODELS`: comma-separated model ids, trimmed, empties
/// dropped.
pub fn parse_model_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_list_is_split_and_trimmed() {
        assert_eq!(
            parse_model_list(" a/A-TEE, b/B-TEE ,,"),
            vec!["a/A-TEE".to_string(), "b/B-TEE".to_string()]
        );
        assert!(parse_model_list(" , ").is_empty());
    }

    #[test]
    fn evidence_is_loaded_from_every_observations_file_below_the_dir() {
        use crate::classify::{Observation, ObservationOutcome};
        let dir = std::env::temp_dir().join(format!("chutes-evidence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run1/artifact")).unwrap();
        std::fs::create_dir_all(dir.join("run2")).unwrap();
        let obs = Observation {
            model: "m".into(),
            chute_id: "c".into(),
            instance_id: "i".into(),
            outcome: ObservationOutcome::Failed {
                stage: "gpu".into(),
            },
        };
        let audit = serde_json::json!({ "date": "2026-10-06", "observations": [obs] });
        std::fs::write(
            dir.join("run1/artifact/observations.json"),
            audit.to_string(),
        )
        .unwrap();
        std::fs::write(dir.join("run2/observations.json"), "not json").unwrap();
        std::fs::write(dir.join("run2/report.md"), "# ignored").unwrap();
        let loaded = load_evidence(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(loaded, vec![obs]);
    }
}
