//! Adversarial / edge-case e2e tests for `x-auto-redact` on
//! /v1/chat/completions, complementing `auto_redact.rs`.
//!
//! These exercise corner cases that aren't covered by the happy-path file:
//! empty inputs, system messages, multimodal parts, dedup, placeholder
//! collision avoidance, and large payloads.

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;

/// Pull `choices[0].message.content` out of a chat completion response as
/// a `String`. Returns empty string if not present.
fn extract_choice_text(value: &serde_json::Value) -> String {
    value
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Register openai/privacy-filter in the cloud-api models DB. Required for
/// auto-redact to route the detector call through the provider pool.
async fn setup_privacy_filter_model(server: &axum_test::TestServer) {
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "openai/privacy-filter".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {"amount": 0, "scale": 9, "currency": "USD"},
            "outputCostPerToken": {"amount": 0, "scale": 9, "currency": "USD"},
            "costPerImage": {"amount": 0, "scale": 9, "currency": "USD"},
            "modelDisplayName": "Privacy Filter",
            "modelDescription": "PII span detection",
            "contextLength": 512,
            "verifiable": false,
            "isActive": true
        }))
        .unwrap(),
    );
    let updated = admin_batch_upsert_models(server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1);
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
}

/// Empty messages array with auto-redact enabled. The route's own
/// `ChatCompletionRequest::validate` rejects this with 400 before redaction
/// runs, so we should never panic or send anything to the provider.
#[tokio::test]
async fn auto_redact_empty_messages_returns_400() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [],
        }))
        .await;

    assert_eq!(
        resp.status_code(),
        400,
        "empty messages array must 400 before redaction runs; got body: {}",
        resp.text()
    );
    // Provider must NOT have been called with PII (or anything).
    assert!(
        mock_provider.last_chat_params().await.is_none(),
        "provider should not have been invoked on a 400-rejected request"
    );
}

/// Empty content with auto-redact enabled. No text fragments means the
/// detector is invoked with an empty list (or short-circuits), no
/// placeholders are minted, and the response is passed through unchanged.
#[tokio::test]
async fn auto_redact_empty_content_is_noop() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new(
            "echoing back",
        ))
        .await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{"role": "user", "content": ""}],
        }))
        .await;

    assert_eq!(
        resp.status_code(),
        200,
        "empty content with auto_redact on should succeed: {}",
        resp.text()
    );

    let body: serde_json::Value = resp.json();
    let content = extract_choice_text(&body);
    assert_eq!(
        content, "echoing back",
        "no PII -> no rewriting; response passes through"
    );
}

/// PII inside a system message must be redacted before it reaches the
/// provider, and un-redacted on the way back. Confirms redaction walks
/// system messages, not only user.
#[tokio::test]
async fn auto_redact_redacts_pii_in_system_message() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new(
            "Will notify <email1>.",
        ))
        .await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [
                {"role": "system", "content": "Always notify admin@corp.com about user activity."},
                {"role": "user", "content": "list current users"}
            ],
        }))
        .await;
    assert_eq!(resp.status_code(), 200, "got: {}", resp.text());

    // Provider must NOT have seen the original email in the system message.
    let params = mock_provider.last_chat_params().await.unwrap();
    let seen = serde_json::to_string(&params.messages).unwrap();
    assert!(
        !seen.contains("admin@corp.com"),
        "raw email leaked to provider in system message: {seen}"
    );
    assert!(
        seen.contains("<email1>"),
        "expected placeholder in system message; got {seen}"
    );

    // Client should see the original email back in the response.
    let body: serde_json::Value = resp.json();
    let content = extract_choice_text(&body);
    assert!(
        content.contains("admin@corp.com"),
        "client should see un-redacted response; got {content}"
    );
}

/// Multimodal content parts: the `text` part is redacted, the `image_url`
/// part is left untouched. PII in tail text is also handled.
#[tokio::test]
async fn auto_redact_handles_multimodal_content_parts() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new(
            "image saved, notify <email1>",
        ))
        .await;

    let image_url = "https://example.com/img.png";
    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "call alice@example.com about this:"},
                    {"type": "image_url", "image_url": {"url": image_url}}
                ]
            }],
        }))
        .await;
    assert_eq!(resp.status_code(), 200, "got: {}", resp.text());

    // Provider must see redacted text and the original image_url part
    // unchanged.
    let params = mock_provider.last_chat_params().await.unwrap();
    let seen = serde_json::to_string(&params.messages).unwrap();
    assert!(
        !seen.contains("alice@example.com"),
        "raw email leaked through multimodal text part: {seen}"
    );
    assert!(
        seen.contains("<email1>"),
        "redacted placeholder missing from multimodal text part: {seen}"
    );
    assert!(
        seen.contains(image_url),
        "image_url must be passed through untouched: {seen}"
    );

    // Response un-redact must restore the original email.
    let body: serde_json::Value = resp.json();
    let content = extract_choice_text(&body);
    assert!(
        content.contains("alice@example.com"),
        "client should see un-redacted response; got {content}"
    );
}

/// Same email occurring twice in the prompt: both occurrences must mint
/// the same placeholder (dedup). The mock response references the
/// placeholder twice; both must be replaced with the original email.
#[tokio::test]
async fn auto_redact_dedups_repeated_email() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new(
            "Both <email1> and <email1> were notified.",
        ))
        .await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{
                "role": "user",
                "content": "ping alice@example.com then ping alice@example.com again"
            }],
        }))
        .await;
    assert_eq!(resp.status_code(), 200, "got: {}", resp.text());

    let params = mock_provider.last_chat_params().await.unwrap();
    let seen = serde_json::to_string(&params.messages).unwrap();

    // Provider should have seen `<email1>` twice (not <email1> and <email2>),
    // and never the raw email.
    assert!(
        !seen.contains("alice@example.com"),
        "raw email leaked: {seen}"
    );
    let email1_count = seen.matches("<email1>").count();
    assert_eq!(
        email1_count, 2,
        "expected <email1> exactly twice (dedup); got {email1_count} in {seen}"
    );
    assert!(
        !seen.contains("<email2>"),
        "dedup failed — saw <email2> minted for same email: {seen}"
    );

    // Client must see the original email twice in the response.
    let body: serde_json::Value = resp.json();
    let content = extract_choice_text(&body);
    let count = content.matches("alice@example.com").count();
    assert_eq!(
        count, 2,
        "client should see original email twice; got {count} in {content}"
    );
    assert!(
        !content.contains("<email1>"),
        "placeholder leaked to client: {content}"
    );
}

/// User prompt contains the literal token `<email1>` AND a real email.
/// The collision-avoidance reserves the literal so we mint `<email2>`
/// instead. The literal `<email1>` must reach the provider unchanged.
#[tokio::test]
async fn auto_redact_avoids_collision_with_user_supplied_placeholder() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Mock response references the placeholder we *expect* the system to
    // mint (`<email2>`, since `<email1>` is already in the input). The
    // unredact map should restore the real email when it sees `<email2>`.
    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new(
            "Got it — emailing <email2> shortly.",
        ))
        .await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{
                "role": "user",
                "content": "I previously wrote <email1>. Please email alice@example.com."
            }],
        }))
        .await;
    assert_eq!(resp.status_code(), 200, "got: {}", resp.text());

    let params = mock_provider.last_chat_params().await.unwrap();
    let seen = serde_json::to_string(&params.messages).unwrap();

    // The raw alice@example.com must be redacted.
    assert!(
        !seen.contains("alice@example.com"),
        "raw email leaked to provider: {seen}"
    );
    // The user's literal `<email1>` must reach the provider unchanged.
    assert!(
        seen.contains("<email1>"),
        "user's literal <email1> must not be eaten: {seen}"
    );
    // The minted placeholder must avoid colliding with <email1>; <email2>
    // is the next ordinal.
    assert!(
        seen.contains("<email2>"),
        "expected collision-avoiding placeholder <email2>; got {seen}"
    );

    // Client must see the real email back; the literal `<email1>` in the
    // response (which was never a minted placeholder) must be left alone.
    let body: serde_json::Value = resp.json();
    let content = extract_choice_text(&body);
    assert!(
        content.contains("alice@example.com"),
        "client should see un-redacted response; got {content}"
    );
    assert!(
        !content.contains("<email2>"),
        "<email2> placeholder leaked to client: {content}"
    );
}

/// Large input (~512 KB of filler with PII at the edges) with auto-redact
/// on. Confirms the redact path handles a substantial body without
/// truncating or mishandling the PII at the boundaries. Stays well under
/// the 25 MB route limit so the request itself doesn't 413.
#[tokio::test]
async fn auto_redact_handles_large_input_under_limit() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new(
            "Done; emailed <email1>.",
        ))
        .await;

    // ~512 KB of benign filler with the PII at the very end so the email
    // regex has to scan the whole text. Use a non-PII-shaped filler so the
    // mock detector emits exactly one span (the email).
    let filler = "x".repeat(512 * 1024);
    let content = format!("{filler} contact alice@example.com");

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{"role": "user", "content": content}],
        }))
        .await;

    assert_eq!(
        resp.status_code(),
        200,
        "large (~512 KB) body under the 25 MB chat limit should succeed; got: {}",
        // Don't dump the entire body — it would be enormous on failure.
        resp.status_code()
    );

    // Provider must NOT have seen the raw email even at the end of a
    // large input.
    let params = mock_provider.last_chat_params().await.unwrap();
    let seen_messages = &params.messages;
    let combined: String = seen_messages
        .iter()
        .filter_map(|m| m.content.as_ref())
        .filter_map(|c| match c {
            serde_json::Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert!(
        !combined.contains("alice@example.com"),
        "raw email leaked at end of large input"
    );
    assert!(
        combined.contains("<email1>"),
        "placeholder missing from large input"
    );

    // Client must see the un-redacted email back.
    let body: serde_json::Value = resp.json();
    let response_content = extract_choice_text(&body);
    assert!(
        response_content.contains("alice@example.com"),
        "client should see un-redacted response; got {response_content}"
    );
}

/// Body that exceeds the 25 MB chat completions limit must be rejected
/// without leaking PII to the provider. The exact status is implementation-
/// defined (axum's DefaultBodyLimit returns 413, but middleware ordering
/// can produce 400 instead); accept either, and require that the provider
/// is never called.
#[tokio::test]
async fn auto_redact_rejects_oversize_body() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new("nope"))
        .await;

    // 26 MB > 25 MB AUDIO_TRANSCRIPTION_MAX_BODY_SIZE limit on
    // /v1/chat/completions.
    let big = "a".repeat(26 * 1024 * 1024);
    let body = serde_json::json!({
        "model": E2E_QWEN_MODEL_NAME,
        "messages": [{
            "role": "user",
            "content": format!("contact alice@example.com {big}"),
        }],
    });

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&body)
        .await;

    let status = resp.status_code();
    assert!(
        status.is_client_error() || status.is_server_error(),
        "oversize body must be rejected; got status {status}"
    );
    assert_ne!(
        status, 200,
        "oversize body must not succeed (would let raw PII reach provider)"
    );

    // Critical privacy invariant: provider must never have been called.
    assert!(
        mock_provider.last_chat_params().await.is_none(),
        "provider must not have been invoked for an oversize request"
    );
}
