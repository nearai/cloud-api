// Single test binary for all e2e tests.
// Each submodule was previously a separate test binary (e2e_*.rs).
// Merging into one binary eliminates ~40 redundant link steps in CI.

use std::fs::OpenOptions;
use std::io::Write;

#[path = "../common/mod.rs"]
mod common;

mod admin_activation_pricing_gate;
mod admin_analytics;
mod admin_deprecate_model;
mod admin_invitation_email_deliveries;
mod admin_list_models;
mod admin_organization_members;
mod admin_pricing_changes;
mod admin_provider_attribution_model_revenue;
mod admin_provider_attribution_platform;
mod admin_provider_attribution_support;
mod admin_schema_compatibility;
mod admin_services;
mod api_keys;
mod attestation_auth;
mod audio_image;
mod audio_transcriptions;
mod auth_tokens;
mod auto_redact;
mod auto_redact_adversarial;
mod backend_output_limits;
mod billing_and_models;
mod chat_encryption;
mod check_api_key;
mod chutes_catalog;
mod client_disconnect;
mod concurrent_limit;
mod conversations;
mod credit_types;
mod cross_workspace;
mod database_encryption;
mod deser_error_envelope;
mod duplicate_names;
mod embeddings;
mod error_msg;
mod external_providers;
mod feature_requests;
mod files;
mod first_stream_event;
mod function_tools;
mod general;
mod glm52_tier_routing;
mod health;
mod invitations;
mod ita_attestation;
mod mcp;
mod mcp_server;
mod message_metadata;
mod model_alias_transparency;
mod model_history_test;
mod multiturn_tools;
mod near_auth;
mod oauth_frontend_callback;
mod openai_tiered_pricing;
mod openrouter_params;
mod org_system_prompt;
mod organization_deletion;
mod pagination_validation;
mod patroni_failover;
mod privacy_classify;
mod privacy_redact;
mod provider_errors;
mod reasoning;
mod reporting_usage;
mod repositories;
mod request_id_contract;
mod rerank;
mod response_signature_verification;
mod score;
mod serving_provider;
mod session_logout;
mod signature_verification;
mod usage_chat_completions;
mod usage_provider_attribution;
mod usage_recording;
mod usage_responses;
mod vpc_login;
mod web_context_search;
mod web_search_citations;

/// Run by nextest's setup script after this E2E binary has already been built.
/// Keeping bootstrap in the same binary avoids a second cold compile/link in CI.
#[tokio::test]
#[ignore = "invoked by the nextest e2e setup script"]
async fn bootstrap_e2e_database() {
    common::db_setup::bootstrap_test_database().await;

    let Some(nextest_env_path) = std::env::var_os("NEXTEST_ENV") else {
        return;
    };
    let nextest_env_path = std::path::PathBuf::from(nextest_env_path);
    assert!(
        nextest_env_path.is_absolute(),
        "NEXTEST_ENV must be an absolute path"
    );

    let mut nextest_env = OpenOptions::new()
        .append(true)
        .open(&nextest_env_path)
        .expect("open nextest's environment file");
    let marker = common::db_setup::nextest_bootstrap_marker()
        .expect("the e2e bootstrap must be invoked by nextest");
    assert!(
        !marker.contains('\r') && !marker.contains('\n'),
        "the e2e bootstrap marker cannot contain a line break"
    );
    writeln!(
        nextest_env,
        "{}={marker}",
        common::db_setup::E2E_DATABASE_BOOTSTRAPPED_ENV,
    )
    .expect("record completed e2e database bootstrap for test processes");
}
