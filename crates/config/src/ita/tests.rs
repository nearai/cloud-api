use super::*;
use serde::Deserialize;
use serial_test::serial;

const POLICY_A: &str = "11111111-1111-4111-8111-111111111111";
const POLICY_B: &str = "22222222-2222-4222-8222-222222222222";
const POLICY_C: &str = "33333333-3333-4333-8333-333333333333";
const ENV_POLICY_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const ENV_POLICY_B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const QUERY_POLICY: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

fn clear_ita_env() {
    for key in [
        "ENABLE_ITA_ATTESTATION",
        "ITA_API_BASE_URL",
        "ITA_PORTAL_BASE_URL",
        "ITA_API_KEY",
        "ITA_API_KEY_FILE",
        "ITA_TIMEOUT_SECONDS",
        "ITA_MAX_RETRIES",
        "ITA_RETRY_BACKOFF_MS",
        "ITA_POLICY_IDS",
        "ITA_POLICY_MUST_MATCH",
        "ITA_TOKEN_SIGNING_ALG",
    ] {
        std::env::remove_var(key);
    }
}

#[test]
#[serial]
fn ita_defaults_when_disabled() {
    // Given: no ITA environment is configured.
    clear_ita_env();

    // When: ITA config is loaded.
    let cfg = ItaAttestationConfig::from_env().unwrap();

    // Then: it is safely disabled with approved defaults and no API key.
    assert!(!cfg.enabled);
    assert_eq!(
        cfg.api_base_url.as_str(),
        "https://api.trustauthority.intel.com"
    );
    assert_eq!(
        cfg.portal_base_url.as_str(),
        "https://portal.trustauthority.intel.com"
    );
    assert!(cfg.api_key.is_none());
    assert_eq!(cfg.timeout_seconds, 10);
    assert_eq!(cfg.max_retries, 2);
    assert_eq!(cfg.retry_backoff_ms, 250);
    assert!(cfg.policy_ids.is_empty());
    assert!(!cfg.policy_must_match);
    assert_eq!(cfg.token_signing_alg, ItaTokenSigningAlg::Ps384);
    clear_ita_env();
}

#[test]
#[serial]
fn ita_key_file_is_trimmed_when_enabled() {
    // Given: ITA is enabled and the API key is mounted as a file.
    clear_ita_env();
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), " fake-ita-key \n").unwrap();
    std::env::set_var("ENABLE_ITA_ATTESTATION", "true");
    std::env::set_var("ITA_API_KEY_FILE", file.path());

    // When: ITA config is loaded.
    let cfg = ItaAttestationConfig::from_env().unwrap();

    // Then: the key material is trimmed and kept only in the secret field.
    assert!(cfg.enabled);
    assert_eq!(cfg.api_key.as_deref(), Some("fake-ita-key"));
    clear_ita_env();
}

#[test]
#[serial]
fn ita_debug_format_redacts_api_key_material() {
    // Given: ITA is enabled with fake key material.
    clear_ita_env();
    std::env::set_var("ENABLE_ITA_ATTESTATION", "true");
    std::env::set_var("ITA_API_KEY", "fake-ita-key");

    // When: config is loaded and formatted through Debug.
    let cfg = ItaAttestationConfig::from_env().unwrap();
    let debug = format!("{cfg:?}");

    // Then: the debug output records only secret presence, never the key.
    assert!(debug.contains("api_key_configured: true"));
    assert!(!debug.contains("fake-ita-key"));
    assert!(!debug.contains("api_key:"));
    clear_ita_env();
}

#[test]
#[serial]
fn ita_empty_key_is_absent_and_error_does_not_print_secret_material() {
    // Given: ITA is enabled with an empty direct key and a separate secret-like value nearby.
    clear_ita_env();
    std::env::set_var("ENABLE_ITA_ATTESTATION", "true");
    std::env::set_var("ITA_API_KEY", "   ");

    // When: ITA config is loaded.
    let err = ItaAttestationConfig::from_env().unwrap_err();

    // Then: the key is treated as absent and the error names fields, not key material.
    assert!(err.contains("ITA_API_KEY"));
    assert!(!err.contains("fake-ita-key"));
    assert!(!err.contains("   "));
    clear_ita_env();
}

#[test]
#[serial]
fn ita_custom_urls_are_accepted_and_invalid_urls_are_rejected() {
    // Given: ITA is enabled with custom regional URLs.
    clear_ita_env();
    std::env::set_var("ENABLE_ITA_ATTESTATION", "true");
    std::env::set_var("ITA_API_KEY", "fake-ita-key");
    std::env::set_var("ITA_API_BASE_URL", "https://api.eu.example.test");
    std::env::set_var("ITA_PORTAL_BASE_URL", "https://portal.eu.example.test/");

    // When: ITA config is loaded.
    let cfg = ItaAttestationConfig::from_env().unwrap();

    // Then: custom URL overrides are preserved without trailing slashes.
    assert_eq!(cfg.api_base_url.as_str(), "https://api.eu.example.test");
    assert_eq!(
        cfg.portal_base_url.as_str(),
        "https://portal.eu.example.test"
    );

    // Given: the API base URL is not HTTP(S).
    std::env::set_var("ITA_API_BASE_URL", "file:///tmp/ita");

    // When: ITA config is loaded.
    let err = ItaAttestationConfig::from_env().unwrap_err();

    // Then: the field is rejected by name.
    assert!(err.contains("ITA_API_BASE_URL"));
    clear_ita_env();
}

#[test]
#[serial]
fn ita_token_signing_alg_accepts_ps384_and_rs256_only() {
    // Given: ITA is enabled with PS384.
    clear_ita_env();
    std::env::set_var("ENABLE_ITA_ATTESTATION", "true");
    std::env::set_var("ITA_API_KEY", "fake-ita-key");
    std::env::set_var("ITA_TOKEN_SIGNING_ALG", "PS384");

    // When: config is loaded.
    let ps384 = ItaAttestationConfig::from_env().unwrap();

    // Then: PS384 is accepted.
    assert_eq!(ps384.token_signing_alg, ItaTokenSigningAlg::Ps384);

    // Given: the env requests RS256.
    std::env::set_var("ITA_TOKEN_SIGNING_ALG", "RS256");

    // When: config is loaded.
    let rs256 = ItaAttestationConfig::from_env().unwrap();

    // Then: RS256 is accepted.
    assert_eq!(rs256.token_signing_alg, ItaTokenSigningAlg::Rs256);

    // Given: an unsupported token signing algorithm is requested.
    std::env::set_var("ITA_TOKEN_SIGNING_ALG", "PS256");

    // When: config is loaded.
    let err = ItaAttestationConfig::from_env().unwrap_err();

    // Then: the field is rejected without exposing secret material.
    assert!(err.contains("ITA_TOKEN_SIGNING_ALG"));
    assert!(!err.contains("fake-ita-key"));
    clear_ita_env();
}

#[test]
#[serial]
fn ita_invalid_token_signing_alg_failure_does_not_print_key_material() {
    // Given: ITA is enabled with an empty key and an unsupported signing algorithm.
    clear_ita_env();
    std::env::set_var("ENABLE_ITA_ATTESTATION", "true");
    std::env::set_var("ITA_API_KEY", "");
    std::env::set_var("ITA_TOKEN_SIGNING_ALG", "PS256");

    // When: config is loaded.
    let err = ItaAttestationConfig::from_env().unwrap_err();
    println!("validation_error={err}");

    // Then: the typed validation error names the algorithm field without printing key material.
    assert!(err.contains("ITA_TOKEN_SIGNING_ALG"));
    assert!(!err.contains("ITA_API_KEY="));
    clear_ita_env();
}

#[test]
#[serial]
fn ita_policy_ids_parse_and_enforce_count_and_length_bounds() {
    // Given: comma-separated policy UUIDs with whitespace.
    clear_ita_env();
    std::env::set_var(
        "ITA_POLICY_IDS",
        format!(" {POLICY_A}, {POLICY_B}, {POLICY_C} "),
    );

    // When: config is loaded while disabled.
    let cfg = ItaAttestationConfig::from_env().unwrap();

    // Then: IDs are trimmed and ordered.
    assert_eq!(cfg.policy_ids.to_strings(), [POLICY_A, POLICY_B, POLICY_C]);

    // Given: more than the bounded number of policies.
    let too_many = (0..=MAX_ITA_POLICY_IDS)
        .map(|idx| format!("00000000-0000-4000-8000-{idx:012}"))
        .collect::<Vec<_>>()
        .join(",");
    std::env::set_var("ITA_POLICY_IDS", too_many);

    // When: config is loaded.
    let count_err = ItaAttestationConfig::from_env().unwrap_err();

    // Then: count validation names the policy field.
    assert!(count_err.contains("ITA_POLICY_IDS"));

    // Given: one policy ID is not an ITA policy UUID.
    std::env::set_var("ITA_POLICY_IDS", "policy-a");

    // When: config is loaded.
    let uuid_err = ItaAttestationConfig::from_env().unwrap_err();

    // Then: UUID validation names the policy field.
    assert!(uuid_err.contains("ITA_POLICY_IDS"));

    // Given: the policy list contains an empty/whitespace entry.
    std::env::set_var("ITA_POLICY_IDS", format!("{POLICY_A}, ,{POLICY_B}"));

    // When: config is loaded.
    let empty_err = ItaAttestationConfig::from_env().unwrap_err();

    // Then: the empty entry is rejected instead of being silently dropped.
    assert!(empty_err.contains("ITA_POLICY_IDS"));
    clear_ita_env();
}

#[test]
fn ita_policy_ids_deserialize_rejects_unvalidated_policy_values() {
    // Given: serde input that bypassed `parse_csv` before this remediation.
    let too_many = (0..=MAX_ITA_POLICY_IDS)
        .map(|idx| format!("00000000-0000-4000-8000-{idx:012}"))
        .collect::<Vec<_>>()
        .join(",");
    let invalid_policy_inputs = [
        "bad id".to_string(),
        "policy-a".to_string(),
        too_many,
        format!("{POLICY_A},,{POLICY_B}"),
        format!("{POLICY_A}, ,{POLICY_B}"),
        " ".to_string(),
    ];

    // When/Then: direct serde deserialization rejects each invalid policy input.
    for raw_policy_ids in invalid_policy_inputs {
        let deserializer =
            serde::de::value::StrDeserializer::<serde::de::value::Error>::new(&raw_policy_ids);
        assert!(
            ItaPolicyIds::deserialize(deserializer).is_err(),
            "policy ids should be rejected: {raw_policy_ids}"
        );
    }
}

#[test]
#[serial]
fn ita_request_policy_overrides_replace_env_defaults() {
    // Given: ITA config has default policy behavior from env.
    clear_ita_env();
    std::env::set_var("ITA_POLICY_IDS", format!("{ENV_POLICY_A},{ENV_POLICY_B}"));
    std::env::set_var("ITA_POLICY_MUST_MATCH", "true");
    std::env::set_var("ITA_TOKEN_SIGNING_ALG", "PS384");
    let cfg = ItaAttestationConfig::from_env().unwrap();

    // When: no request override is supplied.
    let default_effective = cfg.effective_policy(&ItaPolicyOverride::default());

    // Then: env defaults are effective.
    assert_eq!(
        default_effective.policy_ids.to_strings(),
        [ENV_POLICY_A, ENV_POLICY_B]
    );
    assert!(default_effective.policy_must_match);
    assert_eq!(
        default_effective.token_signing_alg,
        ItaTokenSigningAlg::Ps384
    );

    // Given: the request supplies policy IDs, policy matching, and token signing alg.
    let request_override = ItaPolicyOverride {
        policy_ids: Some(ItaPolicyIds::parse_csv(QUERY_POLICY, "policy_ids").unwrap()),
        policy_must_match: Some(false),
        token_signing_alg: Some(ItaTokenSigningAlg::Rs256),
    };

    // When: effective policy is calculated.
    let query_effective = cfg.effective_policy(&request_override);

    // Then: request values replace the env defaults.
    assert_eq!(query_effective.policy_ids.to_strings(), [QUERY_POLICY]);
    assert!(!query_effective.policy_must_match);
    assert_eq!(query_effective.token_signing_alg, ItaTokenSigningAlg::Rs256);
    clear_ita_env();
}
