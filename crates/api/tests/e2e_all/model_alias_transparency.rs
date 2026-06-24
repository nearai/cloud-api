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
            "maxOutputLength": 1024,
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

/// Alias of an *external* model whose backend answers with its upstream
/// model name (`provider_config.model_name` override): the response `model`
/// echo differs from the catalog canonical name, but the warning and header
/// must still fire because alias-ness is derived from catalog resolution of
/// the requested name, not from the echo.
#[tokio::test]
async fn test_alias_to_external_model_with_upstream_name_override_warns() {
    let (server, inference_pool, mock_provider, _) = setup_test_server_with_pool().await;

    let canonical = format!("openai/test-gpt-5-{}", uuid::Uuid::new_v4());
    let upstream = "gpt-5-upstream-snapshot";

    // Canonical external model with an upstream model-name override, plus a
    // synthetic old model deprecated onto it (registers the alias).
    let old = format!("test-alias-ext-old/Old-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        canonical.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken":  { "amount": 2_500_000, "currency": "USD" },
            "outputCostPerToken": { "amount": 10_000_000, "currency": "USD" },
            "modelDisplayName":   "External Override Test Model",
            "modelDescription":   "External model with upstream name override",
            "contextLength":      128000,
            "maxOutputLength": 1024,
            "verifiable":         false,
            "isActive":           true,
            "providerType":       "external",
            "providerConfig": {
                "backend": "openai_compatible",
                "base_url": "https://api.openai.com/v1",
                "model_name": upstream,
            },
            "attestationSupported": false
        }))
        .unwrap(),
    );
    batch.insert(
        old.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken":  { "amount": 1_000_000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2_000_000, "currency": "USD" },
            "modelDisplayName":   "External Override Old Model",
            "modelDescription":   "Synthetic model deprecated onto the external model",
            "contextLength":      4096,
            "maxOutputLength": 1024,
            "verifiable":         false,
            "isActive":           true,
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    let resp = server
        .post("/v1/admin/models/deprecate")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "modelId": old,
            "successorModelId": canonical,
            "changeReason": "alias transparency e2e (external override)"
        }))
        .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());

    // Mock backend echoes the UPSTREAM name, like a real external provider
    // applying provider_config.model_name.
    mock_provider
        .when(inference_providers::mock::RequestMatcher::Any)
        .respond_with(
            inference_providers::mock::ResponseTemplate::new("Hello from upstream")
                .with_model(upstream),
        )
        .await;
    let mock_provider_trait: std::sync::Arc<
        dyn inference_providers::InferenceProvider + Send + Sync,
    > = mock_provider.clone();
    inference_pool
        .register_provider(canonical.clone(), mock_provider_trait)
        .await;

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_body(&old, false))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let body: serde_json::Value = response.json();
    assert_eq!(
        body["model"], upstream,
        "external backend echoes its upstream name"
    );
    let warning = body["warning"].as_str().expect(
        "alias of an external model with upstream-name override must still carry a warning",
    );
    assert!(
        warning.contains(&old) && warning.contains(&canonical),
        "warning should name alias and canonical: {warning}"
    );
    let header = response
        .headers()
        .get("x-model-alias-resolved")
        .expect("alias header must be present despite the upstream echo")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(header, format!("{old} -> {canonical}"));
}

/// Regression for the case where the upstream override string EQUALS the
/// alias: alias `gpt-5.2` -> canonical `openai/gpt-5.2` with
/// `provider_config.model_name = "gpt-5.2"`. The response echo then equals
/// the requested string, so any echo-based detection would stay silent —
/// alias-ness must come from pre-dispatch catalog resolution alone.
#[tokio::test]
async fn test_alias_equal_to_upstream_override_still_warns() {
    let (server, inference_pool, mock_provider, _) = setup_test_server_with_pool().await;

    let suffix = uuid::Uuid::new_v4();
    let bare = format!("test-gpt-bare-{suffix}");
    let canonical = format!("openai/{bare}");

    // Canonical external model whose upstream override AND registered alias
    // are both the bare name.
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        canonical.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken":  { "amount": 2_500_000, "currency": "USD" },
            "outputCostPerToken": { "amount": 10_000_000, "currency": "USD" },
            "modelDisplayName":   "Bare-Alias Override Test Model",
            "modelDescription":   "External model whose alias equals its upstream name",
            "contextLength":      128000,
            "maxOutputLength": 1024,
            "verifiable":         false,
            "isActive":           true,
            "providerType":       "external",
            "providerConfig": {
                "backend": "openai_compatible",
                "base_url": "https://api.openai.com/v1",
                "model_name": bare,
            },
            "aliases": [bare],
            "attestationSupported": false
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // Mock echoes the upstream (== alias) name.
    mock_provider
        .when(inference_providers::mock::RequestMatcher::Any)
        .respond_with(
            inference_providers::mock::ResponseTemplate::new("Hello from bare upstream")
                .with_model(bare.clone()),
        )
        .await;
    let mock_provider_trait: std::sync::Arc<
        dyn inference_providers::InferenceProvider + Send + Sync,
    > = mock_provider.clone();
    inference_pool
        .register_provider(canonical.clone(), mock_provider_trait)
        .await;

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&chat_body(&bare, false))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let body: serde_json::Value = response.json();
    assert_eq!(
        body["model"], bare,
        "echo equals the requested alias string"
    );
    let warning = body["warning"]
        .as_str()
        .expect("alias must warn even when the echo equals the requested string");
    assert!(
        warning.contains(&bare) && warning.contains(&canonical),
        "warning should name alias and canonical: {warning}"
    );
    let header = response
        .headers()
        .get("x-model-alias-resolved")
        .expect("alias header must be present despite echo == requested")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(header, format!("{bare} -> {canonical}"));

    // Strict mode must also reject it.
    let strict = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("x-no-aliasing", "true")
        .json(&chat_body(&bare, false))
        .await;
    assert_eq!(strict.status_code(), 400, "{}", strict.text());
}

/// Legacy /v1/completions dispatches through the same alias-resolving
/// service and must carry the same contract: warning + header on aliased
/// responses, x-no-aliasing rejection.
#[tokio::test]
async fn test_legacy_completions_alias_contract() {
    let server = setup_test_server().await;
    let alias = setup_deprecated_alias(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let body = |stream: bool| {
        serde_json::json!({
            "model": alias,
            "prompt": "Say hello",
            "stream": stream,
            "max_tokens": 16
        })
    };

    // Non-streaming: warning + header
    let response = server
        .post("/v1/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&body(false))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let header = response
        .headers()
        .get("x-model-alias-resolved")
        .expect("aliased legacy completion must carry x-model-alias-resolved")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(header, format!("{alias} -> {E2E_QWEN_MODEL_NAME}"));
    let json: serde_json::Value = response.json();
    let warning = json["warning"]
        .as_str()
        .expect("aliased legacy completion must carry a warning");
    assert!(warning.contains(&alias) && warning.contains(E2E_QWEN_MODEL_NAME));

    // Streaming: header + warning on first chunk only
    let response = server
        .post("/v1/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&body(true))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(response.headers().get("x-model-alias-resolved").is_some());
    let text = response.text();
    let mut data_chunks = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| d.trim() != "[DONE]")
        .map(|d| serde_json::from_str::<serde_json::Value>(d).expect("chunk should parse"));
    let first = data_chunks.next().expect("stream should have chunks");
    assert!(
        first["warning"].as_str().is_some(),
        "first legacy chunk must carry the warning: {first}"
    );
    for chunk in data_chunks {
        assert!(chunk.get("warning").is_none());
    }

    // Strict mode rejects
    let strict = server
        .post("/v1/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("x-no-aliasing", "true")
        .json(&body(false))
        .await;
    assert_eq!(strict.status_code(), 400, "{}", strict.text());
    assert!(strict.text().contains("model_alias_rejected"));

    // Canonical request stays unannotated
    let canonical = server
        .post("/v1/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "prompt": "Say hello",
            "stream": false,
            "max_tokens": 16
        }))
        .await;
    assert_eq!(canonical.status_code(), 200, "{}", canonical.text());
    assert!(canonical.headers().get("x-model-alias-resolved").is_none());
    let json: serde_json::Value = canonical.json();
    assert!(json.get("warning").is_none());
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
