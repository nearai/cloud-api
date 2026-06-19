// E2E tests for the `x-serving-provider` response header (issue #769):
//   - non-streaming completions carry the header with the provider tier string
//   - streaming completions carry the header before the body
//   - GET /v1/attestation/report?provider= selects the correct provider tier
//   - GET /v1/attestation/report?provider=<unknown> returns 400

use crate::common::*;

// ─────────────────────────────────────────────────────────────────────────────
// Header name constant – mirrors the production constant in completions.rs.
// ─────────────────────────────────────────────────────────────────────────────
const X_SERVING_PROVIDER: &str = "x-serving-provider";

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn chat_body_non_streaming(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": "Hello" }],
        "stream": false,
        "max_tokens": 16
    })
}

fn chat_body_streaming(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": "Hello" }],
        "stream": true,
        "max_tokens": 16
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Part 1 – x-serving-provider header in chat completions
// ─────────────────────────────────────────────────────────────────────────────

/// Non-streaming chat completion must carry `x-serving-provider`.
/// The default mock provider uses tier `NonAttested` → value `"non-attested"`.
#[tokio::test]
async fn test_serving_provider_header_non_streaming() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_body_non_streaming(E2E_QWEN_MODEL_NAME))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "expected 200, got: {}",
        response.text()
    );

    let tier = response
        .headers()
        .get(X_SERVING_PROVIDER)
        .expect("non-streaming chat response must carry x-serving-provider")
        .to_str()
        .expect("x-serving-provider must be valid ASCII");

    // The default mock uses ProviderTier::NonAttested → "non-attested"
    assert_eq!(tier, "non-attested", "unexpected serving tier: {tier}");
}

/// Streaming chat completion must carry `x-serving-provider` as a *response header*
/// (set before the body is sent), not inside the SSE payload.
#[tokio::test]
async fn test_serving_provider_header_streaming() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_body_streaming(E2E_QWEN_MODEL_NAME))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "expected 200, got: {}",
        response.text()
    );

    let tier = response
        .headers()
        .get(X_SERVING_PROVIDER)
        .expect("streaming chat response must carry x-serving-provider")
        .to_str()
        .expect("x-serving-provider must be valid ASCII");

    assert_eq!(tier, "non-attested", "unexpected serving tier: {tier}");
}

// ─────────────────────────────────────────────────────────────────────────────
// Part 2 – GET /v1/attestation/report?provider= selector
// ─────────────────────────────────────────────────────────────────────────────

/// `?provider=near` on a model served only by the (NonAttested) default mock
/// must return a non-200 error — no NEAR provider is registered for it.
/// The pool converts `ProviderNotFound` to `ProviderError` which routes to 503.
#[tokio::test]
async fn test_attestation_report_provider_filter_near_not_found() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let encoded_model =
        url::form_urlencoded::byte_serialize(E2E_QWEN_MODEL_NAME.as_bytes()).collect::<String>();
    let url = format!("/v1/attestation/report?model={encoded_model}&provider=near");

    let response = server
        .get(&url)
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_ne!(
        response.status_code(),
        200,
        "expected non-200 when no NEAR provider is registered, got 200: {}",
        response.text()
    );
    // The provider filter was accepted (not a 400 bad request).
    assert_ne!(
        response.status_code(),
        400,
        "?provider=near must not return 400 (it is a valid value)"
    );
}

/// `?provider=chutes` on a model served only by the (NonAttested) default mock
/// must return a non-200 error.
#[tokio::test]
async fn test_attestation_report_provider_filter_chutes_not_found() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let encoded_model =
        url::form_urlencoded::byte_serialize(E2E_QWEN_MODEL_NAME.as_bytes()).collect::<String>();
    let url = format!("/v1/attestation/report?model={encoded_model}&provider=chutes");

    let response = server
        .get(&url)
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_ne!(
        response.status_code(),
        200,
        "expected non-200 when no Chutes provider is registered, got 200: {}",
        response.text()
    );
    assert_ne!(
        response.status_code(),
        400,
        "?provider=chutes must not return 400 (it is a valid value)"
    );
}

/// `?provider=<unknown>` must return 400 with a descriptive error.
#[tokio::test]
async fn test_attestation_report_provider_filter_unknown_returns_400() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let encoded_model =
        url::form_urlencoded::byte_serialize(E2E_QWEN_MODEL_NAME.as_bytes()).collect::<String>();
    let url = format!("/v1/attestation/report?model={encoded_model}&provider=foobar");

    let response = server
        .get(&url)
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "unknown provider value must return 400, got: {}",
        response.text()
    );

    let body = response.text();
    assert!(
        body.contains("foobar"),
        "400 error body should mention the unknown value: {body}"
    );
}

/// Omitting `?provider=` keeps the existing first-successful behaviour
/// (attestation report succeeds, since the NonAttested mock has a registered
/// signing key for the Qwen model).
#[tokio::test]
async fn test_attestation_report_no_provider_filter_succeeds() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let encoded_model =
        url::form_urlencoded::byte_serialize(E2E_QWEN_MODEL_NAME.as_bytes()).collect::<String>();
    let url = format!("/v1/attestation/report?model={encoded_model}");

    let response = server
        .get(&url)
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    // The mock provider returns a report; we only care that the route succeeds.
    assert_eq!(
        response.status_code(),
        200,
        "attestation report without filter must succeed: {}",
        response.text()
    );
}

/// Both `near` and `chutes` values are accepted (case-insensitive) — the
/// filter must not reject them with 400 even when no matching provider exists.
#[tokio::test]
async fn test_attestation_report_provider_filter_case_insensitive() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let encoded_model =
        url::form_urlencoded::byte_serialize(E2E_QWEN_MODEL_NAME.as_bytes()).collect::<String>();

    for value in &["NEAR", "CHUTES", "Near", "Chutes"] {
        let url = format!("/v1/attestation/report?model={encoded_model}&provider={value}");
        let response = server
            .get(&url)
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await;

        // Must parse to a known tier (Near or Attested3p) and not 400.
        // Without a matching provider the model backed only by NonAttested mock
        // will return a non-200 provider error, but NOT a 400 parse error.
        assert_ne!(
            response.status_code(),
            400,
            "case variant '{value}' must be recognised (not 400): {}",
            response.text()
        );
    }
}
