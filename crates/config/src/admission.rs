//! Optional configuration for audience-bound Commons admission assertions.
use crate::types::read_optional_secret_env_absent_empty;

#[derive(Clone)]
pub struct AdmissionProofConfig {
    pub issuer: String,
    pub audiences: Vec<String>,
    pub subject_key: String,
    pub signing_seed: String,
    pub retained_public_keys: Vec<String>,
}

impl std::fmt::Debug for AdmissionProofConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmissionProofConfig")
            .field("issuer", &self.issuer)
            .field("audiences", &self.audiences)
            .field("subject_key", &"[REDACTED]")
            .field("signing_seed", &"[REDACTED]")
            .field("retained_public_keys", &self.retained_public_keys.len())
            .finish()
    }
}

impl AdmissionProofConfig {
    /// Partial configuration is an error. Semantic/crypto validation runs during
    /// auth initialization, before any listener starts.
    pub fn from_env() -> Result<Option<Self>, String> {
        let read = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let issuer = read("ADMISSION_PROOF_ISSUER");
        let audiences = read("ADMISSION_PROOF_AUDIENCES");
        let subject_key = read_optional_secret_env_absent_empty(
            "ADMISSION_PROOF_SUBJECT_KEY_FILE",
            "ADMISSION_PROOF_SUBJECT_KEY",
        )?;
        let signing_seed = read_optional_secret_env_absent_empty(
            "ADMISSION_PROOF_SIGNING_SEED_FILE",
            "ADMISSION_PROOF_SIGNING_SEED",
        )?;
        let retained = read("ADMISSION_PROOF_RETAINED_PUBLIC_KEYS");
        if issuer.is_none()
            && audiences.is_none()
            && subject_key.is_none()
            && signing_seed.is_none()
            && retained.is_none()
        {
            return Ok(None);
        }
        let required = |value: Option<String>, name: &str| {
            value.ok_or_else(|| format!("Admission proofs require {name}"))
        };
        Ok(Some(Self {
            issuer: required(issuer, "ADMISSION_PROOF_ISSUER")?,
            audiences: required(audiences, "ADMISSION_PROOF_AUDIENCES")?
                .split(',')
                .map(str::trim)
                .map(String::from)
                .collect(),
            subject_key: required(subject_key, "ADMISSION_PROOF_SUBJECT_KEY[_FILE]")?,
            signing_seed: required(signing_seed, "ADMISSION_PROOF_SIGNING_SEED[_FILE]")?,
            retained_public_keys: retained
                .map(|keys| keys.split(',').map(str::trim).map(String::from).collect())
                .unwrap_or_default(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::admission::AdmissionProofConfig;

    #[test]
    fn from_env_enforces_complete_configuration_and_secret_file_precedence() {
        // Each case runs in a child process. The test runner's environment is
        // never mutated, including when another config test runs concurrently.
        const CASE: &str = "TC_ADMISSION_ENV_TEST_CASE";
        const PREFIX: &str = "ADMISSION_PROOF_";
        if let Ok(case) = std::env::var(CASE) {
            let result = AdmissionProofConfig::from_env();
            match case.as_str() {
                "absent" => assert!(result.unwrap().is_none()),
                "complete" | "files" => {
                    let config = result.unwrap().unwrap();
                    assert_eq!(config.issuer, "https://cloud.example");
                    assert_eq!(config.audiences, ["commons", "other"]);
                    assert_eq!(config.retained_public_keys, ["public-one", "public-two"]);
                    let expected = if case == "files" { "file" } else { "env" };
                    assert_eq!(config.subject_key, format!("synthetic-{expected}-subject"));
                    assert_eq!(config.signing_seed, format!("synthetic-{expected}-signer"));
                }
                _ => {
                    let error = result.unwrap_err();
                    assert!(!error.contains("synthetic-env-subject"));
                    assert!(!error.contains("synthetic-env-signer"));
                    assert!(error.contains("ADMISSION_PROOF_"));
                }
            }
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let subject = directory.path().join("subject");
        let signer = directory.path().join("signer");
        let empty = directory.path().join("empty");
        std::fs::write(&subject, "synthetic-file-subject\n").unwrap();
        std::fs::write(&signer, "synthetic-file-signer\n").unwrap();
        std::fs::write(&empty, " \n").unwrap();
        let settings = [
            ("ISSUER", "https://cloud.example"),
            ("AUDIENCES", "commons, other"),
            ("SUBJECT_KEY", "synthetic-env-subject"),
            ("SIGNING_SEED", "synthetic-env-signer"),
            ("RETAINED_PUBLIC_KEYS", "public-one,public-two"),
        ];
        for case in [
            "absent",
            "complete",
            "files",
            "ISSUER",
            "AUDIENCES",
            "SUBJECT_KEY",
            "SIGNING_SEED",
            "unreadable",
            "empty-file",
        ] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args(["--exact", "admission::tests::from_env_enforces_complete_configuration_and_secret_file_precedence", "--nocapture"]);
            for name in [
                "ISSUER",
                "AUDIENCES",
                "SUBJECT_KEY",
                "SIGNING_SEED",
                "RETAINED_PUBLIC_KEYS",
                "SUBJECT_KEY_FILE",
                "SIGNING_SEED_FILE",
            ] {
                child.env_remove(format!("{PREFIX}{name}"));
            }
            child.env(CASE, case);
            if case != "absent" {
                for (name, value) in settings {
                    if case != name {
                        child.env(format!("{PREFIX}{name}"), value);
                    }
                }
            }
            match case {
                "files" => {
                    child.env("ADMISSION_PROOF_SUBJECT_KEY_FILE", &subject);
                    child.env("ADMISSION_PROOF_SIGNING_SEED_FILE", &signer);
                }
                "unreadable" => {
                    child.env(
                        "ADMISSION_PROOF_SUBJECT_KEY_FILE",
                        directory.path().join("missing"),
                    );
                }
                "empty-file" => {
                    child.env("ADMISSION_PROOF_SUBJECT_KEY_FILE", &empty);
                }
                _ => {}
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "case {case}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn debug_redacts_private_configuration() {
        let config = AdmissionProofConfig {
            issuer: "https://cloud-api.near.ai".into(),
            audiences: vec!["trace-commons".into()],
            subject_key: "private-subject-key".into(),
            signing_seed: "private-signing-seed".into(),
            retained_public_keys: Vec::new(),
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("private-subject-key"));
        assert!(!debug.contains("private-signing-seed"));
    }
}
