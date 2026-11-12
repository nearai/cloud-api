//! Tests of the MPP settings parsing.
use super::*;
use serial_test::serial;

const KEYS: [&str; 20] = [
    "MPP_ENABLED",
    "MPP_REALM",
    "MPP_SECRET_KEY",
    "MPP_SECRET_KEY_FILE",
    "MPP_SERVICE_API_KEY",
    "MPP_SERVICE_API_KEY_FILE",
    "MPP_CHALLENGE_TTL_SECS",
    "MPP_TEMPO_RPC_URL",
    "MPP_TEMPO_CHAIN_ID",
    "MPP_TEMPO_CURRENCY",
    "MPP_TEMPO_RECIPIENT",
    "MPP_RPC_TIMEOUT_SECS",
    "MPP_MODEL_PRICES",
    "MPP_TEMPO_SIGNER_KEY",
    "MPP_TEMPO_SIGNER_KEY_FILE",
    "MPP_SESSION_MIN_VOUCHER_DELTA",
    "MPP_SESSION_SUGGESTED_DEPOSIT",
    "MPP_SETTLE_INTERVAL_SECS",
    "MPP_SETTLE_AMOUNT",
    "MPP_CLOSE_SUPERVISION_SECS",
];

const SIGNER_KEY: &str = "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";
const SESSION_PRICES: &str = r#"{"zai-org/GLM-5":{"amount":"20000","token_price":"7","max_tokens":2048,"max_request_bytes":65536}}"#;

const SECRET: &str = "0123456789abcdef0123456789abcdef-hmac";
const PRICES: &str =
    r#"{"zai-org/GLM-5":{"amount":"20000","max_tokens":2048,"max_request_bytes":65536}}"#;

fn clear() {
    for key in KEYS {
        std::env::remove_var(key);
    }
}

fn set_valid() {
    clear();
    std::env::set_var("MPP_ENABLED", "true");
    std::env::set_var("MPP_REALM", "cloud-api.near.ai");
    std::env::set_var("MPP_SECRET_KEY", SECRET);
    std::env::set_var("MPP_SERVICE_API_KEY", "sk-live-service-key-0001");
    std::env::set_var("MPP_TEMPO_RPC_URL", "https://rpc.moderato.tempo.xyz");
    std::env::set_var("MPP_TEMPO_CHAIN_ID", "42431");
    std::env::set_var(
        "MPP_TEMPO_CURRENCY",
        "0x20C0000000000000000000000000000000000000",
    );
    std::env::set_var(
        "MPP_TEMPO_RECIPIENT",
        "0xAbCdEf0123456789aBcDeF0123456789AbCdEf01",
    );
    std::env::set_var("MPP_MODEL_PRICES", PRICES);
}

#[test]
#[serial]
fn disabled_by_default_and_reads_nothing_else() {
    clear();
    // Garbage in every other variable must not matter while disabled.
    std::env::set_var("MPP_MODEL_PRICES", "not json");
    std::env::set_var("MPP_SECRET_KEY", "short");
    assert!(MppConfig::from_env().unwrap().settings().is_none());
    std::env::set_var("MPP_ENABLED", "false");
    assert!(MppConfig::from_env().unwrap().settings().is_none());
    clear();
}

#[test]
#[serial]
fn enabled_with_valid_values_normalizes_addresses() {
    set_valid();
    let config = MppConfig::from_env().unwrap();
    let settings = config.settings().unwrap();
    assert_eq!(settings.realm, "cloud-api.near.ai");
    assert_eq!(settings.challenge_ttl_secs, 600);
    assert_eq!(settings.tempo.rpc_timeout_secs, 15);
    assert_eq!(settings.tempo.chain_id, 42431);
    assert_eq!(
        settings.tempo.currency,
        "0x20c0000000000000000000000000000000000000"
    );
    assert_eq!(
        settings.tempo.recipient,
        "0xabcdef0123456789abcdef0123456789abcdef01"
    );
    let price = settings.model_prices["zai-org/GLM-5"];
    assert_eq!(price.amount, 20_000);
    assert_eq!(price.token_price, None);
    assert_eq!(price.max_tokens, 2048);
    assert_eq!(settings.session, None, "no token_price, no session intent");
    clear();
}

#[test]
#[serial]
fn enabled_requires_every_value() {
    for missing in [
        "MPP_REALM",
        "MPP_SECRET_KEY",
        "MPP_SERVICE_API_KEY",
        "MPP_TEMPO_RPC_URL",
        "MPP_TEMPO_CHAIN_ID",
        "MPP_TEMPO_CURRENCY",
        "MPP_TEMPO_RECIPIENT",
        "MPP_MODEL_PRICES",
    ] {
        set_valid();
        std::env::remove_var(missing);
        let error = MppConfig::from_env().unwrap_err();
        assert!(error.contains(missing), "{missing}: {error}");
    }
    clear();
}

#[test]
#[serial]
fn rejects_weak_or_malformed_values() {
    let cases: [(&str, &str, &str); 9] = [
        ("MPP_SECRET_KEY", "too-short", "MPP_SECRET_KEY"),
        (
            "MPP_SERVICE_API_KEY",
            "not-an-sk-key",
            "MPP_SERVICE_API_KEY",
        ),
        ("MPP_REALM", "bad realm!", "MPP_REALM"),
        ("MPP_CHALLENGE_TTL_SECS", "59", "MPP_CHALLENGE_TTL_SECS"),
        ("MPP_CHALLENGE_TTL_SECS", "3601", "MPP_CHALLENGE_TTL_SECS"),
        ("MPP_RPC_TIMEOUT_SECS", "0", "MPP_RPC_TIMEOUT_SECS"),
        ("MPP_TEMPO_CHAIN_ID", "0", "MPP_TEMPO_CHAIN_ID"),
        ("MPP_TEMPO_CURRENCY", "0x1234", "MPP_TEMPO_CURRENCY"),
        (
            "MPP_TEMPO_RPC_URL",
            "http://rpc.example.com",
            "MPP_TEMPO_RPC_URL",
        ),
    ];
    for (key, value, expected) in cases {
        set_valid();
        std::env::set_var(key, value);
        let error = MppConfig::from_env().unwrap_err();
        assert!(error.contains(expected), "{key}: {error}");
    }
    clear();
}

#[test]
#[serial]
fn ttl_bounds_are_inclusive() {
    for ttl in ["60", "3600"] {
        set_valid();
        std::env::set_var("MPP_CHALLENGE_TTL_SECS", ttl);
        assert!(MppConfig::from_env().is_ok(), "{ttl}");
    }
    clear();
}

#[test]
#[serial]
fn secrets_load_from_files() {
    set_valid();
    std::env::remove_var("MPP_SECRET_KEY");
    std::env::remove_var("MPP_SERVICE_API_KEY");
    let dir = tempfile::tempdir().unwrap();
    let secret_path = dir.path().join("secret");
    let key_path = dir.path().join("key");
    std::fs::write(&secret_path, format!("{SECRET}\n")).unwrap();
    std::fs::write(&key_path, "sk-live-from-file-0002\n").unwrap();
    std::env::set_var("MPP_SECRET_KEY_FILE", &secret_path);
    std::env::set_var("MPP_SERVICE_API_KEY_FILE", &key_path);
    let config = MppConfig::from_env().unwrap();
    let settings = config.settings().unwrap();
    assert_eq!(settings.secret_key, SECRET);
    assert_eq!(settings.service_api_key, "sk-live-from-file-0002");
    clear();
}

#[test]
#[serial]
fn debug_redacts_secrets_and_rpc_url() {
    set_valid();
    std::env::set_var(
        "MPP_TEMPO_RPC_URL",
        "https://rpc.example.com/v1/provider-key-xyz",
    );
    let config = MppConfig::from_env().unwrap();
    let debug = format!("{config:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains(SECRET));
    assert!(!debug.contains("sk-live-service-key-0001"));
    assert!(!debug.contains("provider-key-xyz"));
    clear();
}

#[test]
fn rpc_url_allows_https_and_loopback_http_only() {
    assert!(parse_rpc_url("https://rpc.moderato.tempo.xyz").is_ok());
    assert!(parse_rpc_url("http://127.0.0.1:8545").is_ok());
    assert!(parse_rpc_url("http://localhost:8545/").is_ok());
    assert!(parse_rpc_url("http://[::1]:8545").is_ok());
    assert!(parse_rpc_url("http://rpc.example.com").is_err());
    assert!(parse_rpc_url("http://127.0.0.1.evil.example").is_err());
    assert!(parse_rpc_url("ftp://localhost").is_err());
    assert!(parse_rpc_url("not a url").is_err());
}

#[test]
fn model_prices_parse_strictly() {
    let ok = parse_model_prices(PRICES).unwrap();
    assert_eq!(ok.len(), 1);

    for (label, raw) in [
        ("empty table", "{}"),
        ("not json", "nope"),
        ("array", "[]"),
        (
            "unknown field",
            r#"{"m":{"amount":"1","max_tokens":1,"max_request_bytes":1,"fee":1}}"#,
        ),
        ("missing field", r#"{"m":{"amount":"1","max_tokens":1}}"#),
        (
            "zero amount",
            r#"{"m":{"amount":"0","max_tokens":1,"max_request_bytes":1}}"#,
        ),
        (
            "numeric amount",
            r#"{"m":{"amount":5,"max_tokens":1,"max_request_bytes":1}}"#,
        ),
        (
            "signed amount",
            r#"{"m":{"amount":"+5","max_tokens":1,"max_request_bytes":1}}"#,
        ),
        (
            "fractional amount",
            r#"{"m":{"amount":"1.5","max_tokens":1,"max_request_bytes":1}}"#,
        ),
        (
            "amount over u128",
            r#"{"m":{"amount":"340282366920938463463374607431768211456","max_tokens":1,"max_request_bytes":1}}"#,
        ),
        (
            "zero token cap",
            r#"{"m":{"amount":"1","max_tokens":0,"max_request_bytes":1}}"#,
        ),
        (
            "zero size cap",
            r#"{"m":{"amount":"1","max_tokens":1,"max_request_bytes":0}}"#,
        ),
        (
            "padded model name",
            r#"{" m":{"amount":"1","max_tokens":1,"max_request_bytes":1}}"#,
        ),
        (
            "duplicate model",
            r#"{"m":{"amount":"1","max_tokens":1,"max_request_bytes":1},"m":{"amount":"2","max_tokens":1,"max_request_bytes":1}}"#,
        ),
    ] {
        assert!(parse_model_prices(raw).is_err(), "{label} must be rejected");
    }
}

#[test]
fn u128_max_amount_is_accepted() {
    let raw = r#"{"m":{"amount":"340282366920938463463374607431768211455","max_tokens":1,"max_request_bytes":1}}"#;
    assert_eq!(parse_model_prices(raw).unwrap()["m"].amount, u128::MAX);
}

#[test]
fn realm_must_be_an_ascii_host() {
    assert!(parse_realm("cloud-api.near.ai").is_ok());
    assert!(parse_realm("localhost").is_ok());
    for bad in ["", ".x", "x.", "-x", "a b", "a\"b", "é.com", "a/b", "a:80"] {
        assert!(parse_realm(bad).is_err(), "{bad}");
    }
}

fn set_session() {
    set_valid();
    std::env::set_var("MPP_MODEL_PRICES", SESSION_PRICES);
    std::env::set_var("MPP_TEMPO_SIGNER_KEY", SIGNER_KEY);
}

fn session_error() -> String {
    MppConfig::from_env().unwrap_err()
}

#[test]
#[serial]
fn session_settings_take_their_defaults() {
    set_session();
    let config = MppConfig::from_env().unwrap();
    let settings = config.settings().unwrap();
    let session = settings.session.as_ref().unwrap();
    assert_eq!(session.signer_key.as_deref(), Some(SIGNER_KEY));
    assert_eq!(session.min_voucher_delta, None);
    assert_eq!(session.suggested_deposit, 1_000_000);
    assert_eq!(session.settle_interval_secs, 3600);
    assert_eq!(session.settle_amount, 5_000_000);
    assert_eq!(session.close_supervision_secs, 60);
    let price = settings.model_prices["zai-org/GLM-5"];
    assert_eq!((price.amount, price.token_price), (20_000, Some(7)));
    clear();
}

#[test]
#[serial]
fn session_settings_read_every_variable() {
    set_session();
    std::env::set_var("MPP_TEMPO_SIGNER_KEY", format!("0x{SIGNER_KEY}"));
    std::env::set_var("MPP_SESSION_MIN_VOUCHER_DELTA", "448");
    std::env::set_var("MPP_SESSION_SUGGESTED_DEPOSIT", "2500000");
    std::env::set_var("MPP_SETTLE_INTERVAL_SECS", "600");
    std::env::set_var("MPP_SETTLE_AMOUNT", "1");
    std::env::set_var("MPP_CLOSE_SUPERVISION_SECS", "10");
    let config = MppConfig::from_env().unwrap();
    let session = config.settings().unwrap().session.clone().unwrap();
    assert_eq!(
        session.signer_key.as_deref(),
        Some(format!("0x{SIGNER_KEY}").as_str())
    );
    assert_eq!(session.min_voucher_delta, Some(448));
    assert_eq!(session.suggested_deposit, 2_500_000);
    assert_eq!(session.settle_interval_secs, 600);
    assert_eq!(session.settle_amount, 1);
    assert_eq!(session.close_supervision_secs, 10);
    clear();
}

#[test]
#[serial]
fn a_token_price_makes_the_signer_key_required() {
    set_session();
    std::env::remove_var("MPP_TEMPO_SIGNER_KEY");
    let error = session_error();
    assert!(error.contains("MPP_TEMPO_SIGNER_KEY"), "{error}");
    // Without a token_price the key is optional, and the session intent is off.
    set_valid();
    let config = MppConfig::from_env().unwrap();
    assert!(config.settings().unwrap().session.is_none());
    clear();
}

#[test]
#[serial]
fn the_signer_key_loads_from_a_file_and_both_sources_conflict() {
    set_session();
    std::env::remove_var("MPP_TEMPO_SIGNER_KEY");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("signer");
    std::fs::write(&path, format!("  {SIGNER_KEY}\n")).unwrap();
    std::env::set_var("MPP_TEMPO_SIGNER_KEY_FILE", &path);
    let config = MppConfig::from_env().unwrap();
    let session = config.settings().unwrap().session.clone().unwrap();
    assert_eq!(session.signer_key.as_deref(), Some(SIGNER_KEY));

    std::env::set_var("MPP_TEMPO_SIGNER_KEY", SIGNER_KEY);
    let error = session_error();
    assert!(error.contains("MPP_TEMPO_SIGNER_KEY"), "{error}");
    assert!(error.contains("only one"), "{error}");

    std::env::remove_var("MPP_TEMPO_SIGNER_KEY");
    std::env::set_var("MPP_TEMPO_SIGNER_KEY_FILE", dir.path().join("missing"));
    let error = session_error();
    assert!(error.contains("MPP_TEMPO_SIGNER_KEY_FILE"), "{error}");

    // An empty file is no key at all.
    std::fs::write(&path, "\n").unwrap();
    std::env::set_var("MPP_TEMPO_SIGNER_KEY_FILE", &path);
    let error = session_error();
    assert!(error.contains("must be set"), "{error}");
    clear();
}

#[test]
#[serial]
fn the_signer_key_shape_is_checked_and_never_echoed() {
    let wrong_shapes = [
        "not-hex-at-all-not-hex-at-all-not-hex-at-all-not-hex-at-all-1234".to_string(),
        "abcd".to_string(),
        SIGNER_KEY[..62].to_string(),
        format!("{SIGNER_KEY}00"),
        format!("0x0x{}", &SIGNER_KEY[..60]),
        format!("zz{}", &SIGNER_KEY[2..]),
    ];
    for wrong in &wrong_shapes {
        set_session();
        std::env::set_var("MPP_TEMPO_SIGNER_KEY", wrong);
        let error = session_error();
        assert!(error.contains("MPP_TEMPO_SIGNER_KEY"), "{error}");
        assert!(
            !error.contains(wrong.as_str()),
            "the value must not be echoed"
        );
    }
    // The scalar itself (all zero) is the signer's concern, not the config's.
    set_session();
    std::env::set_var("MPP_TEMPO_SIGNER_KEY", "0".repeat(64));
    assert!(MppConfig::from_env().is_ok());
    clear();
}

#[test]
#[serial]
fn an_unused_signer_key_is_still_checked() {
    set_valid();
    std::env::set_var("MPP_TEMPO_SIGNER_KEY", "short");
    assert!(session_error().contains("MPP_TEMPO_SIGNER_KEY"));
    clear();
}

#[test]
#[serial]
fn session_tuning_bounds_and_errors() {
    let cases: [(&str, &str); 22] = [
        ("MPP_SESSION_MIN_VOUCHER_DELTA", "0"),
        ("MPP_SESSION_MIN_VOUCHER_DELTA", "-1"),
        ("MPP_SESSION_MIN_VOUCHER_DELTA", "1.5"),
        (
            "MPP_SESSION_MIN_VOUCHER_DELTA",
            "340282366920938463463374607431768211456",
        ),
        ("MPP_SESSION_SUGGESTED_DEPOSIT", "0"),
        ("MPP_SESSION_SUGGESTED_DEPOSIT", "ten"),
        (
            "MPP_SESSION_SUGGESTED_DEPOSIT",
            "79228162514264337593543950336",
        ),
        ("MPP_SETTLE_INTERVAL_SECS", "59"),
        ("MPP_SETTLE_INTERVAL_SECS", "0"),
        ("MPP_SETTLE_INTERVAL_SECS", "hourly"),
        ("MPP_SETTLE_INTERVAL_SECS", "-60"),
        ("MPP_SETTLE_AMOUNT", "0"),
        ("MPP_SETTLE_AMOUNT", "+5"),
        ("MPP_SETTLE_AMOUNT", "5e6"),
        ("MPP_CLOSE_SUPERVISION_SECS", "9"),
        ("MPP_CLOSE_SUPERVISION_SECS", "301"),
        ("MPP_CLOSE_SUPERVISION_SECS", "0"),
        ("MPP_CLOSE_SUPERVISION_SECS", "soon"),
        ("MPP_SESSION_SUGGESTED_DEPOSIT", " 12 3"),
        ("MPP_SETTLE_AMOUNT", "1_000"),
        ("MPP_SESSION_MIN_VOUCHER_DELTA", "0x10"),
        ("MPP_CLOSE_SUPERVISION_SECS", "60s"),
    ];
    for (key, value) in cases {
        set_session();
        std::env::set_var(key, value);
        let error = session_error();
        assert!(error.contains(key), "{key}={value}: {error}");
    }
    // Inclusive edges.
    for (key, value) in [
        ("MPP_SETTLE_INTERVAL_SECS", "60"),
        ("MPP_CLOSE_SUPERVISION_SECS", "10"),
        ("MPP_CLOSE_SUPERVISION_SECS", "300"),
        (
            "MPP_SETTLE_AMOUNT",
            "340282366920938463463374607431768211455",
        ),
        (
            "MPP_SESSION_SUGGESTED_DEPOSIT",
            "79228162514264337593543950335",
        ),
        ("MPP_SESSION_MIN_VOUCHER_DELTA", "1"),
    ] {
        set_session();
        std::env::set_var(key, value);
        assert!(MppConfig::from_env().is_ok(), "{key}={value}");
    }
    clear();
}

#[test]
#[serial]
fn session_tuning_is_validated_even_when_sessions_are_off() {
    set_valid();
    std::env::set_var("MPP_SETTLE_INTERVAL_SECS", "1");
    assert!(session_error().contains("MPP_SETTLE_INTERVAL_SECS"));
    clear();
}

#[test]
#[serial]
fn session_variables_are_not_read_while_mpp_is_disabled() {
    clear();
    std::env::set_var("MPP_SETTLE_AMOUNT", "0");
    std::env::set_var("MPP_TEMPO_SIGNER_KEY", "short");
    assert!(MppConfig::from_env().unwrap().settings().is_none());
    clear();
}

#[test]
#[serial]
fn debug_redacts_the_signer_key() {
    set_session();
    let config = MppConfig::from_env().unwrap();
    let debug = format!("{config:?}");
    assert!(!debug.contains(SIGNER_KEY));
    assert!(debug.contains("MppSessionSettings"));
    assert!(debug.contains("<redacted>"));
    clear();
}

#[test]
fn model_prices_accept_amount_token_price_or_both() {
    let parse = |fields: &str| {
        parse_model_prices(&format!(
            r#"{{"m":{{{fields}"max_tokens":9,"max_request_bytes":99}}}}"#
        ))
    };
    let both = parse(r#""amount":"5","token_price":"2","#).unwrap()["m"];
    assert_eq!((both.amount, both.token_price), (5, Some(2)));
    let only_amount = parse(r#""amount":"5","#).unwrap()["m"];
    assert_eq!((only_amount.amount, only_amount.token_price), (5, None));
    let only_token = parse(r#""token_price":"2","#).unwrap()["m"];
    assert_eq!((only_token.amount, only_token.token_price), (0, Some(2)));
    assert_eq!(
        (only_token.max_tokens, only_token.max_request_bytes),
        (9, 99)
    );

    for (label, fields) in [
        ("neither", ""),
        ("zero token_price", r#""token_price":"0","#),
        ("numeric token_price", r#""token_price":2,"#),
        ("signed token_price", r#""token_price":"+2","#),
        ("fractional token_price", r#""token_price":"0.5","#),
        ("empty token_price", r#""token_price":"","#),
        ("null token_price", r#""token_price":null,"#),
        ("null amount", r#""amount":null,"token_price":"2","#),
        (
            "token_price over u128",
            r#""token_price":"340282366920938463463374607431768211456","#,
        ),
        (
            "zero amount beside token_price",
            r#""amount":"0","token_price":"2","#,
        ),
        (
            "duplicate token_price",
            r#""token_price":"1","token_price":"2","#,
        ),
    ] {
        assert!(parse(fields).is_err(), "{label} must be rejected");
    }
    // The caps are required for a token-priced model too.
    assert!(parse_model_prices(r#"{"m":{"token_price":"2","max_tokens":9}}"#).is_err());
    assert!(parse_model_prices(
        r#"{"m":{"token_price":"2","max_tokens":0,"max_request_bytes":9}}"#
    )
    .is_err());
    // The error names the model and the field, never a value.
    let error = parse(r#""token_price":"0","#).unwrap_err();
    assert!(
        error.contains("token_price") && error.contains('m'),
        "{error}"
    );
}
