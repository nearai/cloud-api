//! Error mapping and usage settlement for /v1/privacy/classify and
//! /v1/privacy/redact, driven through scripted upstream outcomes
//! (`MockProvider::set_privacy_classify_override`).
//!
//! Each test pins the client-visible status, error JSON, and recorded
//! `input_tokens` for one upstream shape.

use crate::common::*;
use inference_providers::mock::PrivacyClassifyOverride;

const CLASSIFY: &str = "/v1/privacy/classify";
const REDACT: &str = "/v1/privacy/redact";

async fn setup() -> (
    axum_test::TestServer,
    std::sync::Arc<inference_providers::mock::MockProvider>,
    String,
    String,
) {
    let (server, _pool, mock, _db) = setup_test_server_with_pool().await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let org_id = org.id.clone();
    let api_key = get_api_key_for_org(&server, org.id).await;
    (server, mock, org_id, api_key)
}

async fn post(
    server: &axum_test::TestServer,
    path: &str,
    api_key: &str,
    input: serde_json::Value,
) -> axum_test::TestResponse {
    server
        .post(path)
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": E2E_PRIVACY_FILTER_MODEL_NAME,
            "input": input,
        }))
        .await
}

/// `input_tokens` of every usage row recorded for the org, newest first.
async fn recorded_input_tokens(server: &axum_test::TestServer, org_id: &str) -> Vec<i32> {
    let resp = server
        .get(&format!(
            "/v1/organizations/{org_id}/usage/history?limit=20&offset=0"
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let history: api::routes::usage::UsageHistoryResponse = resp.json();
    history.data.iter().map(|e| e.input_tokens).collect()
}

fn error_parts(resp: &axum_test::TestResponse) -> (u16, String, String) {
    let body: serde_json::Value = resp.json();
    (
        resp.status_code().as_u16(),
        body["error"]["type"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

async fn upstream_http_error(mock: &inference_providers::mock::MockProvider, status: u16) {
    mock.set_privacy_classify_override(Some(PrivacyClassifyOverride::HttpError {
        status_code: status,
        message: "upstream body".to_string(),
    }))
    .await;
}

async fn upstream_body(mock: &inference_providers::mock::MockProvider, body: serde_json::Value) {
    mock.set_privacy_classify_override(Some(PrivacyClassifyOverride::Body(body)))
        .await;
}

// ---- upstream HTTP errors ----

/// Client-visible `(status, error.type, error.message)` for each upstream
/// HTTP status. These used to be uniformly 502 `server_error` "try again
/// later": the pool collapsed every failure into `RequestFailed`, so a 4xx
/// caused by the client's payload looked like an outage and invited retries.
/// The pool now keeps the upstream status (with a sanitized message), and
/// the handlers classify it as embeddings does.
fn expected_for_upstream(status: u16, generic: &str) -> (u16, String, String) {
    let detector = format!("PII detector returned HTTP {status}");
    match status {
        400 | 422 => (status, "invalid_request_error".into(), detector),
        404 => (404, "not_found_error".into(), detector),
        429 => (
            429,
            "rate_limit_error".into(),
            "Rate limit exceeded by upstream provider. Please retry with exponential backoff."
                .into(),
        ),
        503 => (
            429,
            "service_overloaded".into(),
            "All inference backends are overloaded. Please retry with exponential backoff.".into(),
        ),
        _ => (500, "server_error".into(), generic.into()),
    }
}

async fn assert_upstream_error_mapping(path: &str, generic: &str) {
    let (server, mock, org_id, api_key) = setup().await;
    for status in [400u16, 401, 403, 404, 422, 429, 500, 502, 503] {
        upstream_http_error(&mock, status).await;

        let resp = post(&server, path, &api_key, serde_json::json!("hi")).await;

        assert_eq!(
            error_parts(&resp),
            expected_for_upstream(status, generic),
            "{path} upstream {status}"
        );
    }
    assert!(recorded_input_tokens(&server, &org_id).await.is_empty());
}

#[tokio::test]
async fn classify_upstream_http_error_mapping() {
    assert_upstream_error_mapping(
        CLASSIFY,
        "Privacy classify request failed. Please try again later.",
    )
    .await;
}

#[tokio::test]
async fn redact_upstream_http_error_mapping() {
    assert_upstream_error_mapping(
        REDACT,
        "Privacy redact request failed. Please try again later.",
    )
    .await;
}

// ---- usage settlement ----

#[tokio::test]
async fn classify_sums_same_index_entries() {
    let (server, mock, org_id, api_key) = setup().await;
    upstream_body(
        &mock,
        serde_json::json!({
            "model": E2E_PRIVACY_FILTER_MODEL_NAME,
            "data": [
                {"index": 0, "spans": [], "usage": {"input_tokens": 5}},
                {"index": 0, "spans": [], "usage": {"input_tokens": 7}},
            ]
        }),
    )
    .await;

    let resp = post(&server, CLASSIFY, &api_key, serde_json::json!("hi")).await;

    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert_eq!(recorded_input_tokens(&server, &org_id).await, vec![12]);
}

/// Redact used to keep only the last same-index entry (billing 7 here)
/// while classify billed 12 for the identical upstream response. Same-index
/// entries are legitimate splits of one input, so redact now sums them too
/// and reports the summed per-input usage.
#[tokio::test]
async fn redact_sums_same_index_entries() {
    let (server, mock, org_id, api_key) = setup().await;
    upstream_body(
        &mock,
        serde_json::json!({
            "model": E2E_PRIVACY_FILTER_MODEL_NAME,
            "data": [
                {"index": 0, "spans": [], "usage": {"input_tokens": 5}},
                {"index": 0, "spans": [], "usage": {"input_tokens": 7}},
            ]
        }),
    )
    .await;

    let resp = post(&server, REDACT, &api_key, serde_json::json!("hi")).await;

    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body: serde_json::Value = resp.json();
    assert_eq!(body["data"][0]["usage"]["input_tokens"], 12);
    assert_eq!(recorded_input_tokens(&server, &org_id).await, vec![12]);
}

/// One entry's `input_tokens` is not a number. It used to fail the whole
/// typed usage parse, billing 0 for the call and firing a zero-token
/// anomaly. Parsing is now lenient per entry (as `detect::parse_response`
/// is), so only that entry is skipped and the other one bills.
fn usage_with_one_non_numeric_entry() -> serde_json::Value {
    serde_json::json!({
        "model": E2E_PRIVACY_FILTER_MODEL_NAME,
        "data": [
            {"index": 0, "spans": [], "usage": {"input_tokens": 5}},
            {"index": 1, "spans": [], "usage": {"input_tokens": "n/a"}},
        ]
    })
}

/// One entry's `input_tokens` does not fit i32. It used to fail the whole
/// parse (billing 0); it now counts as i64 and the total hits the
/// MAX_REASONABLE_TOKENS cap like any other oversized report.
fn usage_with_one_oversized_entry() -> serde_json::Value {
    serde_json::json!({
        "model": E2E_PRIVACY_FILTER_MODEL_NAME,
        "data": [
            {"index": 0, "spans": [], "usage": {"input_tokens": 5}},
            {"index": 1, "spans": [], "usage": {"input_tokens": 3_000_000_000i64}},
        ]
    })
}

#[tokio::test]
async fn classify_malformed_usage_entry_skips_only_that_entry() {
    let (server, mock, org_id, api_key) = setup().await;
    upstream_body(&mock, usage_with_one_non_numeric_entry()).await;

    let resp = post(&server, CLASSIFY, &api_key, serde_json::json!(["a", "b"])).await;

    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert_eq!(recorded_input_tokens(&server, &org_id).await, vec![5]);
}

#[tokio::test]
async fn classify_oversized_usage_entry_is_capped() {
    let (server, mock, org_id, api_key) = setup().await;
    upstream_body(&mock, usage_with_one_oversized_entry()).await;

    let resp = post(&server, CLASSIFY, &api_key, serde_json::json!(["a", "b"])).await;

    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert_eq!(
        recorded_input_tokens(&server, &org_id).await,
        vec![1_000_000]
    );
}

#[tokio::test]
async fn redact_malformed_usage_entry_skips_only_that_entry() {
    let (server, mock, org_id, api_key) = setup().await;
    upstream_body(&mock, usage_with_one_non_numeric_entry()).await;

    let resp = post(&server, REDACT, &api_key, serde_json::json!(["a", "b"])).await;

    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body: serde_json::Value = resp.json();
    assert_eq!(
        body["data"][0]["usage"],
        serde_json::json!({"input_tokens": 5})
    );
    assert_eq!(body["data"][1]["usage"], serde_json::json!({}));
    assert_eq!(recorded_input_tokens(&server, &org_id).await, vec![5]);
}

#[tokio::test]
async fn redact_oversized_usage_entry_is_capped() {
    let (server, mock, org_id, api_key) = setup().await;
    upstream_body(&mock, usage_with_one_oversized_entry()).await;

    let resp = post(&server, REDACT, &api_key, serde_json::json!(["a", "b"])).await;

    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body: serde_json::Value = resp.json();
    assert_eq!(
        body["data"][0]["usage"],
        serde_json::json!({"input_tokens": 5})
    );
    assert_eq!(
        body["data"][1]["usage"],
        serde_json::json!({"input_tokens": 3_000_000_000i64})
    );
    assert_eq!(
        recorded_input_tokens(&server, &org_id).await,
        vec![1_000_000]
    );
}

#[tokio::test]
async fn classify_caps_unreasonable_token_count() {
    let (server, mock, org_id, api_key) = setup().await;
    upstream_body(
        &mock,
        serde_json::json!({
            "data": [
                {"index": 0, "spans": [], "usage": {"input_tokens": 900_000}},
                {"index": 1, "spans": [], "usage": {"input_tokens": 900_000}},
            ]
        }),
    )
    .await;

    let resp = post(&server, CLASSIFY, &api_key, serde_json::json!(["a", "b"])).await;

    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert_eq!(
        recorded_input_tokens(&server, &org_id).await,
        vec![1_000_000]
    );
}

#[tokio::test]
async fn redact_caps_unreasonable_token_count() {
    let (server, mock, org_id, api_key) = setup().await;
    upstream_body(
        &mock,
        serde_json::json!({
            "data": [
                {"index": 0, "spans": [], "usage": {"input_tokens": 900_000}},
                {"index": 1, "spans": [], "usage": {"input_tokens": 900_000}},
            ]
        }),
    )
    .await;

    let resp = post(&server, REDACT, &api_key, serde_json::json!(["a", "b"])).await;

    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert_eq!(
        recorded_input_tokens(&server, &org_id).await,
        vec![1_000_000]
    );
}

#[tokio::test]
async fn redact_ignores_out_of_range_index_usage() {
    let (server, mock, org_id, api_key) = setup().await;
    upstream_body(
        &mock,
        serde_json::json!({
            "data": [
                {"index": 0, "spans": [], "usage": {"input_tokens": 4}},
                {"index": 5, "spans": [], "usage": {"input_tokens": 100}},
            ]
        }),
    )
    .await;

    let resp = post(&server, REDACT, &api_key, serde_json::json!("hi")).await;

    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert_eq!(recorded_input_tokens(&server, &org_id).await, vec![4]);
}
