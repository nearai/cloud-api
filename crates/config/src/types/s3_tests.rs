use super::*;
use serial_test::serial;
use std::ffi::OsString;

struct Environment(Vec<(&'static str, Option<OsString>)>);

impl Environment {
    fn new() -> Self {
        let keys = [
            "S3_MOCK",
            "AWS_S3_BUCKET",
            "AWS_S3_REGION",
            "S3_ENCRYPTION_KEYS",
            "S3_ENCRYPTION_KEY",
            "S3_ENCRYPTION_KEY_FILE",
            "S3_ENCRYPTION_ALLOW_INLINE_KEYS",
            "OBJECT_REWRAP_MAX_BYTES",
            "OBJECT_ENCRYPTION_WRITE_ENABLED",
        ];
        let saved = Self(keys.iter().map(|key| (*key, env::var_os(key))).collect());
        for key in keys {
            env::remove_var(key);
        }
        env::set_var("AWS_S3_BUCKET", "test-bucket");
        env::set_var("AWS_S3_REGION", "test-region");
        saved
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => env::set_var(key, value),
                None => env::remove_var(key),
            }
        }
    }
}

#[test]
#[serial]
fn ring_configuration_replaces_legacy_key_selection() {
    let _env = Environment::new();
    env::set_var(
        "S3_ENCRYPTION_KEYS",
        "s3-v2=dstack:/keys/v2,s3-v1=file:/config/object-key",
    );
    let config = S3Config::from_env().unwrap();
    assert!(config.encryption_key.is_empty());
    assert_eq!(
        config
            .encryption_keys
            .unwrap()
            .iter()
            .map(|spec| spec.key_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s3-v2", "s3-v1"]
    );
    env::set_var("S3_ENCRYPTION_KEY_FILE", "/missing/legacy-key");
    env::set_var("S3_ENCRYPTION_KEY", "invalid-legacy-key");
    assert!(S3Config::from_env().is_ok());
}

#[test]
#[serial]
fn inline_ring_keys_require_explicit_s3_opt_in() {
    let _env = Environment::new();
    let key = "07".repeat(32);
    env::set_var(
        "S3_ENCRYPTION_KEYS",
        format!("s3-v2=file:/keys/v2,s3-v1=hex:{key}"),
    );
    for opt_in in [None, Some("false"), Some("1"), Some("TRUE")] {
        match opt_in {
            Some(value) => env::set_var("S3_ENCRYPTION_ALLOW_INLINE_KEYS", value),
            None => env::remove_var("S3_ENCRYPTION_ALLOW_INLINE_KEYS"),
        }
        let error = S3Config::from_env().unwrap_err();
        assert!(error.contains("S3_ENCRYPTION_KEYS entry 1"));
        assert!(error.contains("S3_ENCRYPTION_ALLOW_INLINE_KEYS=true"));
        assert!(!error.contains(&key));
    }
}

#[test]
#[serial]
fn inline_ring_keys_load_when_s3_opt_in_is_true() {
    let _env = Environment::new();
    env::set_var(
        "S3_ENCRYPTION_KEYS",
        format!("s3-v1=hex:{}", "07".repeat(32)),
    );
    env::set_var("S3_ENCRYPTION_ALLOW_INLINE_KEYS", "true");
    let config = S3Config::from_env().unwrap();
    assert!(config.encryption_key.is_empty());
    assert!(matches!(
        config.encryption_keys.unwrap()[0].source,
        KeySource::Hex(_)
    ));
}

#[test]
#[serial]
fn legacy_key_is_required_validated_and_omitted_from_debug() {
    let _env = Environment::new();
    assert!(S3Config::from_env().is_err());
    for invalid in ["private-key-text".into(), "11".repeat(31), "gg".repeat(32)] {
        env::set_var("S3_ENCRYPTION_KEY", &invalid);
        let error = S3Config::from_env().unwrap_err();
        assert!(!error.contains(&invalid));
    }
    let key = "09".repeat(32);
    env::set_var("S3_ENCRYPTION_KEY", &key);
    let config = S3Config::from_env().unwrap();
    assert_eq!(config.encryption_key, key);
    assert!(config.encryption_keys.is_none());
    assert!(!format!("{config:?}").contains(&key));
    env::set_var("S3_ENCRYPTION_KEYS", "");
    assert!(S3Config::from_env().unwrap().encryption_keys.is_none());
}

#[test]
#[serial]
fn legacy_file_retains_precedence_and_trims_whitespace() {
    let _env = Environment::new();
    let file = tempfile::NamedTempFile::new().unwrap();
    let key = "07".repeat(32);
    std::fs::write(file.path(), format!(" {key}\n")).unwrap();
    env::set_var("S3_ENCRYPTION_KEY_FILE", file.path());
    env::set_var("S3_ENCRYPTION_KEY", "invalid-inline-key");
    assert_eq!(S3Config::from_env().unwrap().encryption_key, key);
    std::fs::write(file.path(), "short").unwrap();
    assert!(S3Config::from_env().is_err());
}

#[test]
#[serial]
fn invalid_ring_configuration_names_the_s3_variable() {
    let _env = Environment::new();
    env::set_var("S3_ENCRYPTION_KEYS", "s3-v1=hex:private-key-text");
    let error = S3Config::from_env().unwrap_err();
    assert!(error.contains("S3_ENCRYPTION_KEYS entry 0"));
    assert!(!error.contains("private-key-text"));
}

#[test]
#[serial]
fn object_rewrap_limit_rejects_nonpositive_or_malformed_values() {
    let _env = Environment::new();
    env::set_var("S3_ENCRYPTION_KEYS", "s3-v1=file:/keys/current");
    for value in ["0", "-1", "", "1.5", "256MiB", "18446744073709551616"] {
        env::set_var("OBJECT_REWRAP_MAX_BYTES", value);
        let error = S3Config::from_env().unwrap_err();
        assert!(error.contains("OBJECT_REWRAP_MAX_BYTES"));
    }
}

#[test]
#[serial]
fn object_rewrap_write_gate_rejects_non_boolean_values() {
    let _env = Environment::new();
    env::set_var("S3_ENCRYPTION_KEYS", "s3-v1=file:/keys/current");
    for value in ["", "2", "truefalse", "disabled"] {
        env::set_var("OBJECT_ENCRYPTION_WRITE_ENABLED", value);
        let error = S3Config::from_env().unwrap_err();
        assert!(error.contains("OBJECT_ENCRYPTION_WRITE_ENABLED"));
    }
}

#[test]
#[serial]
fn object_rewrap_settings_preserve_defaults_when_unset() {
    let _env = Environment::new();
    env::set_var("S3_ENCRYPTION_KEYS", "s3-v1=file:/keys/current");
    let config = S3Config::from_env().unwrap();
    assert_eq!(config.object_rewrap_max_bytes, 268_435_456);
    assert!(config.object_encryption_write_enabled);
}

#[test]
#[serial]
fn object_rewrap_settings_accept_explicit_limit_and_disabled_gate() {
    let _env = Environment::new();
    env::set_var("S3_ENCRYPTION_KEYS", "s3-v1=file:/keys/current");
    env::set_var("OBJECT_REWRAP_MAX_BYTES", "1");
    env::set_var("OBJECT_ENCRYPTION_WRITE_ENABLED", "false");
    let config = S3Config::from_env().unwrap();
    assert_eq!(config.object_rewrap_max_bytes, 1);
    assert!(!config.object_encryption_write_enabled);
}

#[test]
#[serial]
fn object_rewrap_limit_accepts_surrounding_whitespace() {
    let _env = Environment::new();
    env::set_var("S3_ENCRYPTION_KEYS", "s3-v1=file:/keys/current");
    env::set_var("OBJECT_REWRAP_MAX_BYTES", " \t42\n");
    assert_eq!(S3Config::from_env().unwrap().object_rewrap_max_bytes, 42);
}

#[test]
#[serial]
fn object_rewrap_gate_accepts_the_shared_boolean_syntax() {
    let _env = Environment::new();
    env::set_var("S3_ENCRYPTION_KEYS", "s3-v1=file:/keys/current");
    for (value, expected) in [
        ("1", true),
        ("0", false),
        (" TRUE ", true),
        ("\tFaLsE\n", false),
    ] {
        env::set_var("OBJECT_ENCRYPTION_WRITE_ENABLED", value);
        assert_eq!(
            S3Config::from_env()
                .unwrap()
                .object_encryption_write_enabled,
            expected,
            "value={value:?}"
        );
    }
}
