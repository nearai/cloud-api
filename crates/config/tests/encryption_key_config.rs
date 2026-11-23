use config::ApiConfig;
use std::process::{Command, Output};

fn load_config(extra: &[(&str, &str)]) -> Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "config_probe", "--nocapture"])
        .env_clear()
        .env("CONFIG_PROBE", "1")
        .env("AUTH_ENCODING_KEY", "test-auth")
        .env("DATABASE_PASSWORD", "test-password")
        .env("DATABASE_NAME", "test-db")
        .env("DATABASE_USERNAME", "test-user")
        .env("DATABASE_CONNECTION_MODE", "direct")
        .env("DATABASE_HOST", "localhost")
        .env("S3_ENCRYPTION_KEY", "ab".repeat(32))
        .env("AWS_S3_BUCKET", "test-bucket")
        .env("AWS_S3_REGION", "us-east-1")
        .envs(extra.iter().copied())
        .output()
        .unwrap()
}

#[test]
fn config_probe() {
    if std::env::var_os("CONFIG_PROBE").is_none() {
        return;
    }
    let config = ApiConfig::from_env().unwrap();
    if config.database_encryption_keys.is_some() {
        assert!(config.database_encryption_key.is_empty());
        assert_eq!(config.database_encryption_key_id, "db-v2");
    } else {
        assert_eq!(config.database_encryption_key, "test-legacy");
        assert_eq!(config.database_encryption_key_id, "db-v1");
    }
}

#[test]
fn ring_mode_starts_with_missing_legacy_file() {
    // Given a ring and a legacy file that cannot be read.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("removed-legacy-key");
    // When startup loads the ring configuration.
    let output = load_config(&[
        ("DB_ENCRYPTION_KEYS", "db-v2=dstack:/keys/v2"),
        ("DB_ENCRYPTION_KEY_FILE", path.to_str().unwrap()),
    ]);
    // Then the removed legacy source does not prevent startup.
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn ring_mode_ignores_legacy_values_and_warns_once() {
    // Given legacy values beside a write-first ring.
    let output = load_config(&[
        ("DB_ENCRYPTION_KEYS", "db-v2=dstack:/keys/v2"),
        ("DB_ENCRYPTION_KEY", "secret-legacy-key"),
        ("DB_ENCRYPTION_KEY_ID", "ignored-legacy-id"),
    ]);
    // When startup loads those variables, then only the ring determines the key/id.
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.contains("DB_ENCRYPTION_KEYS"))
            .count(),
        1
    );
    assert!(!stderr.contains("secret-legacy-key"));
    assert!(!stderr.contains("ignored-legacy-id"));
}

#[test]
fn legacy_mode_preserves_key_and_default_id() {
    // Given no ring, when startup loads the legacy key, then its default id stays db-v1.
    let output = load_config(&[("DB_ENCRYPTION_KEY", "test-legacy")]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn inline_ring_keys_require_explicit_opt_in_without_exposing_values() {
    let key = "ab".repeat(32);
    let raw = format!("db-v2=dstack:/keys/v2,db-v1=hex:{key}");
    for opt_in in [None, Some("false"), Some("TRUE"), Some("1")] {
        let mut extra = vec![("DB_ENCRYPTION_KEYS", raw.as_str())];
        if let Some(value) = opt_in {
            extra.push(("DB_ENCRYPTION_ALLOW_INLINE_KEYS", value));
        }
        let output = load_config(&extra);
        assert!(!output.status.success(), "inline keys must require true");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("DB_ENCRYPTION_KEYS entry 1"), "{stderr}");
        assert!(
            stderr.contains("DB_ENCRYPTION_ALLOW_INLINE_KEYS"),
            "{stderr}"
        );
        assert!(!stderr.contains(&key));
        assert!(!stderr.contains(&raw));
    }
}

#[test]
fn inline_ring_keys_load_with_explicit_opt_in() {
    let raw = format!("db-v2=hex:{}", "cd".repeat(32));
    let output = load_config(&[
        ("DB_ENCRYPTION_KEYS", &raw),
        ("DB_ENCRYPTION_ALLOW_INLINE_KEYS", "true"),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
