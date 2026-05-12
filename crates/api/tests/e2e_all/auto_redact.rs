//! E2E tests for `x-auto-redact` on /v1/chat/completions.
//!
//! The MockProvider's `privacy_classify_raw` does shape-based PII detection
//! (email, SSN, phone), so we can exercise the full redact→provider→
//! unredact loop in-process without a live privacy-filter model.

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;

/// Pull `choices[0].message.content` out of a chat completion response as
/// a `&str`. Returns empty string if the path isn't there — tests assert
/// non-empty content downstream.
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

#[tokio::test]
async fn auto_redact_off_passes_pii_through() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new("ok"))
        .await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{
                "role": "user",
                "content": "My email is alice@example.com"
            }],
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    // With auto-redact off, the mock should have seen the raw PII.
    let params = mock_provider
        .last_chat_params()
        .await
        .expect("mock should have recorded a chat call");
    let seen = serde_json::to_string(&params.messages).unwrap();
    assert!(
        seen.contains("alice@example.com"),
        "expected unredacted PII to reach provider when auto_redact is off; got {seen}"
    );
}

#[tokio::test]
async fn auto_redact_header_redacts_prompt_and_restores_response() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Mock response references the minted placeholder so we can verify the
    // un-redact path puts the original back.
    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new(
            "I'll email <email1> shortly.",
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
                "content": "Please reach out to alice@example.com"
            }],
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    // Provider must NOT have seen the original email.
    let params = mock_provider
        .last_chat_params()
        .await
        .expect("recorded call");
    let seen = serde_json::to_string(&params.messages).unwrap();
    assert!(
        !seen.contains("alice@example.com"),
        "provider saw raw PII: {seen}"
    );
    assert!(
        seen.contains("<email1>"),
        "provider should see placeholder: {seen}"
    );

    // Client must see the original PII in the response.
    let body: serde_json::Value = resp.json();
    let content = extract_choice_text(&body);
    assert!(
        content.contains("alice@example.com"),
        "client should see un-redacted response; got {content}"
    );
    assert!(
        !content.contains("<email1>"),
        "placeholder should have been swapped back; got {content}"
    );
}

#[tokio::test]
async fn auto_redact_body_field_equivalent_to_header() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new(
            "noted <email1>",
        ))
        .await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{ "role": "user", "content": "ping bob@example.com" }],
            "auto_redact": true,
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    // Provider saw placeholder, client gets original back.
    let params = mock_provider.last_chat_params().await.unwrap();
    let seen = serde_json::to_string(&params.messages).unwrap();
    assert!(!seen.contains("bob@example.com"));
    assert!(seen.contains("<email1>"));

    // The body field must be stripped from extra so it isn't forwarded
    // upstream — providers like Anthropic 422 on unknown fields.
    let extra_keys: Vec<&String> = params.extra.keys().collect();
    assert!(
        !extra_keys.iter().any(|k| k.as_str() == "auto_redact"),
        "auto_redact body field must be stripped before forwarding; saw keys {extra_keys:?}"
    );

    let body: serde_json::Value = resp.json();
    let content = extract_choice_text(&body);
    assert!(
        content.contains("bob@example.com"),
        "client should see un-redacted response; got {content}"
    );
}

#[tokio::test]
async fn auto_redact_streaming_splits_placeholder_across_chunks() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // The MockProvider's default streaming behavior emits the response
    // character-by-character, so a placeholder like `<email1>` will be
    // split across SSE chunks — exactly the case the StreamUnredact tail
    // is designed for.
    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new(
            "Sending to <email1> now.",
        ))
        .await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{ "role": "user", "content": "email alice@example.com" }],
            "stream": true,
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    // Concatenate all `delta.content` from the SSE stream.
    let body_text = resp.text();
    let mut assembled = String::new();
    for line in body_text.lines() {
        let payload = match line.strip_prefix("data: ") {
            Some(p) => p,
            None => continue,
        };
        if payload == "[DONE]" {
            continue;
        }
        let Ok(chunk) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        if let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) {
            for ch in choices {
                if let Some(content) = ch
                    .get("delta")
                    .and_then(|d| d.get("content"))
                    .and_then(|c| c.as_str())
                {
                    assembled.push_str(content);
                }
            }
        }
    }

    assert!(
        assembled.contains("alice@example.com"),
        "streamed content should be un-redacted; got: {assembled:?}"
    );
    assert!(
        !assembled.contains("<email1>"),
        "no placeholder should leak to client; got: {assembled:?}"
    );
}

#[tokio::test]
async fn auto_redact_fail_closed_when_pii_model_missing() {
    let (server, pool, _mock, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    // Force the PII detector to be unavailable by unregistering its mock
    // entry from the pool. With no provider for openai/privacy-filter, the
    // detector call must fail and the handler must refuse the request
    // rather than send raw PII to the provider.
    pool.unregister_provider("openai/privacy-filter").await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{ "role": "user", "content": "email alice@example.com" }],
        }))
        .await;
    assert_eq!(
        resp.status_code(),
        503,
        "must fail closed when PII detector is unavailable"
    );
    let body: serde_json::Value = resp.json();
    assert_eq!(
        body["error"]["type"].as_str(),
        Some("auto_redact_unavailable"),
        "expected typed error: {body}"
    );
}

#[tokio::test]
async fn auto_redact_off_when_header_is_falsy() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new("ok"))
        .await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "off")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{
                "role": "user",
                "content": "My SSN is 555-66-7777"
            }],
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    let params = mock_provider.last_chat_params().await.unwrap();
    let seen = serde_json::to_string(&params.messages).unwrap();
    assert!(
        seen.contains("555-66-7777"),
        "off header must not redact: {seen}"
    );
}

#[tokio::test]
async fn auto_redact_multiple_pii_kinds_per_message() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new(
            "Got it — emailing <email1> and texting <phone1>; SSN <account1> on file.",
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
                "content": "contact alice@example.com phone +1-555-123-4567 ssn 555-66-7777"
            }],
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    let params = mock_provider.last_chat_params().await.unwrap();
    let seen = serde_json::to_string(&params.messages).unwrap();
    for raw in ["alice@example.com", "+1-555-123-4567", "555-66-7777"] {
        assert!(!seen.contains(raw), "raw {raw} leaked to provider: {seen}");
    }
    assert!(seen.contains("<email1>"));
    assert!(seen.contains("<phone1>"));
    assert!(seen.contains("<account1>"));

    let body: serde_json::Value = resp.json();
    let content = extract_choice_text(&body);
    for raw in ["alice@example.com", "+1-555-123-4567", "555-66-7777"] {
        assert!(
            content.contains(raw),
            "client should see raw {raw}; got: {content}"
        );
    }
}
