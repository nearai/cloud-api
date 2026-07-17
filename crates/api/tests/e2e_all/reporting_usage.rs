use crate::common::{
    get_session_id, setup_test_server_with_config, setup_test_server_with_database, MOCK_USER_AGENT,
};
use chrono::{Duration, Utc};
use database::Database;
use serde_json::Value;
use std::sync::Arc;

mod token_auth;
mod token_management;
mod usage_export;
mod usage_export_fixture;
mod usage_summary;

include!("reporting_usage/session_history.rs");

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn assert_json_has_no_secret_fields(value: &Value) {
    let text = value.to_string();
    assert!(
        !text.contains("token_hash"),
        "JSON response must not expose token_hash: {text}"
    );
    assert!(
        !text.contains("raw_token"),
        "JSON response must not expose raw_token: {text}"
    );
    assert_no_raw_token_field(value);
}

fn assert_no_raw_token_field(value: &Value) {
    match value {
        Value::Object(map) => {
            assert!(
                !map.contains_key("token"),
                "JSON response must not expose raw token field: {value}"
            );
            for child in map.values() {
                assert_no_raw_token_field(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                assert_no_raw_token_field(child);
            }
        }
        _ => {}
    }
}

fn redact_create_response(value: &Value) -> Value {
    let mut redacted = value.clone();
    if let Value::Object(map) = &mut redacted {
        map.insert(
            "token".to_string(),
            Value::String("rpt-<redacted>".to_string()),
        );
    }
    redacted
}

async fn setup_reporting_usage_server() -> (axum_test::TestServer, Arc<Database>) {
    init_reporting_usage_tracing();
    setup_test_server_with_database().await
}

#[tokio::test]
async fn disabled_reporting_hides_query_routes_but_keeps_management_and_openapi() {
    let server = setup_test_server_with_config(|config| {
        config.usage_reporting.enabled = false;
    })
    .await;
    let org_id = uuid::Uuid::new_v4();

    let export = server
        .get(format!("/v1/organizations/{org_id}/usage/export").as_str())
        .await;
    assert_eq!(export.status_code(), 404, "{}", export.text());

    let management = server
        .get(format!("/v1/organizations/{org_id}/reporting-tokens").as_str())
        .await;
    assert_eq!(management.status_code(), 401, "{}", management.text());

    let openapi = server.get("/api-docs/openapi.json").await;
    assert_eq!(openapi.status_code(), 200, "{}", openapi.text());
    assert!(openapi.text().contains("/usage/export"));
}

fn init_reporting_usage_tracing() {
    let filter = tracing_subscriber::EnvFilter::new("debug,tokio_postgres=info");
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter(filter)
        .try_init();
}

async fn create_reporting_token(server: &axum_test::TestServer, org_id: &str) -> String {
    let response = server
        .post(format!("/v1/organizations/{org_id}/reporting-tokens").as_str())
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "name": "reporting auth probe",
            "expires_at": (Utc::now() + Duration::days(30)).to_rfc3339(),
        }))
        .await;

    assert_eq!(response.status_code(), 201, "{}", response.text());
    response
        .json::<Value>()
        .get("token")
        .and_then(Value::as_str)
        .expect("create response should include raw reporting token once")
        .to_string()
}
