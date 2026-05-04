// Single test binary for all e2e tests.
// Each submodule was previously a separate test binary (e2e_*.rs).
// Merging into one binary eliminates ~40 redundant link steps in CI.

#[path = "../common/mod.rs"]
mod common;

mod admin_analytics;
mod admin_deprecate_model;
mod admin_list_models;
mod admin_services;
mod api_keys;
mod audio_image;
mod audio_transcriptions;
mod auth_tokens;
mod billing_and_models;
mod chat_encryption;
mod check_api_key;
mod client_disconnect;
mod concurrent_limit;
mod conversations;
mod credit_types;
mod duplicate_names;
mod embeddings;
mod error_msg;
mod external_providers;
mod files;
mod function_tools;
mod general;
mod mcp;
mod mcp_server;
mod message_metadata;
mod model_history_test;
mod multiturn_tools;
mod near_auth;
mod oauth_frontend_callback;
mod org_system_prompt;
mod pagination_validation;
mod provider_errors;
mod repositories;
mod rerank;
mod response_signature_verification;
mod score;
mod signature_verification;
mod usage_chat_completions;
mod usage_recording;
mod usage_responses;
mod vpc_login;
mod web_search_citations;
