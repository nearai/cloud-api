// E2E tests for alias-substitution transparency (issue #573):
// - aliased chat completions carry a top-level "warning" and an
//   `x-model-alias-resolved` response header,
// - the `x-no-aliasing` request header rejects aliased requests with 400
//   before any inference happens.

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;

/// Create a synthetic model and deprecate it in favor of the e2e Qwen mock
/// model, returning the deprecated (alias) name. This reproduces the exact
/// production path from issue #573: `POST /v1/admin/models/deprecate`
/// registers the old name as an alias of the successor.
async fn setup_deprecated_alias(server: &axum_test::TestServer) -> String {
    setup_qwen_model(server).await;

    let old = format!("test-alias-old/Old-Model-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        old.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken":  { "amount": 1_000_000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2_000_000, "currency": "USD" },
            "modelDisplayName":   "Alias Transparency Test Model",
            "modelDescription":   "Synthetic model deprecated onto Qwen for e2e",
            "contextLength":      4096,
            "verifiable":         false,
            "isActive":           true,
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(server, batch, get_session_id()).await;

    let resp = server
        .post("/v1/admin/models/deprecate")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "modelId": old,
            "successorModelId": E2E_QWEN_MODEL_NAME,
            "changeReason": "alias transparency e2e"
        }))
        .await;
    assert_eq!(
        resp.status_code(),
        200,
        "deprecation should succeed: {}",
        resp.text()
    );
    old
}

fn chat_body(model: &str, stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": "Hello" }],
        "stream": stream,
        "max_tokens": 16
    })
}

#[tokio::test]
async fn test_aliased_request_warns_non_streaming() {
    let server = setup_test_server().await;
    let alias = setup_deprecated_alias(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_body(&alias, false))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    // Response header announces the substitution
    let header = response
        .headers()
        .get("x-model-alias-resolved")
        .expect("aliased response must carry x-model-alias-resolved")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(header, format!("{alias} -> {E2E_QWEN_MODEL_NAME}"));

    // Body carries the canonical model and a top-level warning
    let body: serde_json::Value = response.json();
    assert_eq!(body["model"], E2E_QWEN_MODEL_NAME);
    let warning = body["warning"]
        .as_str()
        .expect("aliased response must carry a top-level warning");
    assert!(
        warning.contains(&alias) && warning.contains(E2E_QWEN_MODEL_NAME),
        "warning should name both alias and canonical model: {warning}"
    );
}

#[tokio::test]
async fn test_aliased_request_warns_streaming_first_chunk() {
    let server = setup_test_server().await;
    let alias = setup_deprecated_alias(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_body(&alias, true))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let header = response
        .headers()
        .get("x-model-alias-resolved")
        .expect("aliased stream must carry x-model-alias-resolved")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(header, format!("{alias} -> {E2E_QWEN_MODEL_NAME}"));

    // Only the FIRST data chunk carries the warning
    let text = response.text();
    let mut data_chunks = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| d.trim() != "[DONE]")
        .map(|d| serde_json::from_str::<serde_json::Value>(d).expect("chunk should parse"));

    let first = data_chunks.next().expect("stream should have chunks");
    let warning = first["warning"]
        .as_str()
        .expect("first chunk of aliased stream must carry a warning");
    assert!(
        warning.contains(&alias) && warning.contains(E2E_QWEN_MODEL_NAME),
        "warning should name both alias and canonical model: {warning}"
    );
    assert_eq!(first["model"], E2E_QWEN_MODEL_NAME);

    for chunk in data_chunks {
        assert!(
            chunk.get("warning").is_none(),
            "only the first chunk should carry the warning, got: {chunk}"
        );
    }
}

#[tokio::test]
async fn test_no_aliasing_header_rejects_aliased_request() {
    let server = setup_test_server().await;
    let alias = setup_deprecated_alias(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    for value in ["true", "1", ""] {
        let response = server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .add_header("x-no-aliasing", value)
            .json(&chat_body(&alias, false))
            .await;
        assert_eq!(
            response.status_code(),
            400,
            "x-no-aliasing={value:?} on an alias must 400, got: {}",
            response.text()
        );
        let body = response.text();
        assert!(
            body.contains("model_alias_rejected") && body.contains(E2E_QWEN_MODEL_NAME),
            "rejection should carry the code and canonical name: {body}"
        );
    }

    // Explicit opt-out value still serves (with warning)
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("x-no-aliasing", "false")
        .json(&chat_body(&alias, false))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
}

#[tokio::test]
async fn test_no_aliasing_header_allows_canonical_request() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("x-no-aliasing", "true")
        .json(&chat_body(E2E_QWEN_MODEL_NAME, false))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(response.headers().get("x-model-alias-resolved").is_none());
    let body: serde_json::Value = response.json();
    assert!(body.get("warning").is_none());
}

#[tokio::test]
async fn test_attestation_report_announces_alias() {
    let server = setup_test_server().await;
    let alias = setup_deprecated_alias(&server).await;

    let encoded = url::form_urlencoded::byte_serialize(alias.as_bytes()).collect::<String>();
    let response = server
        .get(&format!("/v1/attestation/report?model={encoded}"))
        .await;
    // The mock attestation path may or may not produce a full report, but
    // whenever the request is served the alias header must be present.
    if response.status_code() == 200 {
        let header = response
            .headers()
            .get("x-model-alias-resolved")
            .expect("aliased attestation report must carry x-model-alias-resolved")
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(header, format!("{alias} -> {E2E_QWEN_MODEL_NAME}"));
    }
}

#[tokio::test]
async fn test_attestation_report_no_aliasing_rejects() {
    let server = setup_test_server().await;
    let alias = setup_deprecated_alias(&server).await;

    let encoded = url::form_urlencoded::byte_serialize(alias.as_bytes()).collect::<String>();
    let response = server
        .get(&format!("/v1/attestation/report?model={encoded}"))
        .add_header("x-no-aliasing", "true")
        .await;
    assert_eq!(
        response.status_code(),
        400,
        "x-no-aliasing on an aliased attestation request must 400, got: {}",
        response.text()
    );
    assert!(
        response.text().contains(E2E_QWEN_MODEL_NAME),
        "rejection should name the canonical model: {}",
        response.text()
    );
}

#[tokio::test]
async fn test_non_aliased_request_unannotated() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_body(E2E_QWEN_MODEL_NAME, false))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(
        response.headers().get("x-model-alias-resolved").is_none(),
        "non-aliased responses must not carry the alias header"
    );
    let body: serde_json::Value = response.json();
    assert!(
        body.get("warning").is_none(),
        "non-aliased responses must not carry a warning"
    );
}
