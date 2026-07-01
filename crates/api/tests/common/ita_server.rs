use std::sync::OnceLock;

use super::{fake_ita::FakeIta, setup_test_server_with_config_and_ita_model_evidence};

static DEV_ENV: OnceLock<()> = OnceLock::new();

pub async fn setup_ita_server(fake_ita: &FakeIta, mode: ItaServerMode) -> axum_test::TestServer {
    setup_ita_server_with_policy(fake_ita, mode, "").await
}

pub async fn setup_ita_server_with_env_policy(
    fake_ita: &FakeIta,
    policy_ids: &str,
) -> axum_test::TestServer {
    setup_ita_server_with_policy(
        fake_ita,
        ItaServerMode::Enabled { max_retries: 0 },
        policy_ids,
    )
    .await
}

async fn setup_ita_server_with_policy(
    fake_ita: &FakeIta,
    mode: ItaServerMode,
    policy_ids: &str,
) -> axum_test::TestServer {
    allow_debug_attestation_keys();
    setup_test_server_with_config_and_ita_model_evidence(|config| {
        config.ita = match mode {
            ItaServerMode::Disabled => config::ItaAttestationConfig::default(),
            ItaServerMode::Enabled { max_retries } => ita_config(fake_ita, max_retries, policy_ids),
        };
    })
    .await
}

fn allow_debug_attestation_keys() {
    DEV_ENV.get_or_init(|| {
        std::env::set_var("DEV", "1");
        std::env::set_var("BRAVE_SEARCH_PRO_API_KEY", "ita-attestation-e2e");
    });
}

fn ita_config(
    fake_ita: &FakeIta,
    max_retries: u32,
    policy_ids: &str,
) -> config::ItaAttestationConfig {
    config::ItaAttestationConfig {
        enabled: true,
        api_base_url: config::ItaBaseUrl::parse(&fake_ita.base_url, "ITA_API_BASE_URL")
            .expect("fake ITA base URL should be valid"),
        portal_base_url: config::ItaBaseUrl::parse(
            "https://portal.example.test",
            "ITA_PORTAL_BASE_URL",
        )
        .expect("test portal base URL should be valid"),
        api_key: Some("test-api-key".to_string()),
        timeout_seconds: 1,
        max_retries,
        retry_backoff_ms: 1,
        policy_ids: config::ItaPolicyIds::parse_csv(policy_ids, "ITA_POLICY_IDS")
            .expect("test policy ids should be valid"),
        policy_must_match: false,
        token_signing_alg: config::ItaTokenSigningAlg::Ps384,
    }
}

#[derive(Clone, Copy)]
pub enum ItaServerMode {
    Disabled,
    Enabled { max_retries: u32 },
}
