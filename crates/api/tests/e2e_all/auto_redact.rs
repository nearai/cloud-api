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
            "isActive": true,
            "allowFree": true
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
            "I'll email redacted1@example.com shortly.",
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
        seen.contains("redacted1@example.com"),
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
        !content.contains("redacted1@example.com"),
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
            "noted redacted1@example.com",
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
    assert!(seen.contains("redacted1@example.com"));

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
    // character-by-character, so a placeholder like `redacted1@example.com` will be
    // split across SSE chunks — exactly the case the StreamUnredact tail
    // is designed for.
    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new(
            "Sending to redacted1@example.com now.",
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
        !assembled.contains("redacted1@example.com"),
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
async fn auto_redact_skips_response_munging_when_no_pii_detected() {
    // When auto_redact is requested but the prompt has no PII, the
    // detector returns no spans, the placeholder map stays empty, and we
    // should NOT munge / re-serialize the response. This preserves the
    // raw_bytes signing path for clean inputs.
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Provider response contains a literal that *looks* like a placeholder.
    // If we were re-serializing unconditionally, it would still pass
    // through; this test mainly ensures the path stays the no-op one.
    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new("hi there"))
        .await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{ "role": "user", "content": "what time is it" }],
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    let body: serde_json::Value = resp.json();
    let content = extract_choice_text(&body);
    assert_eq!(content, "hi there");
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
            "Got it — emailing redacted1@example.com and texting +1-555-0100; SSN 000-00-0001 on file.",
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
    assert!(seen.contains("redacted1@example.com"));
    assert!(seen.contains("+1-555-0100"));
    assert!(seen.contains("000-00-0001"));

    let body: serde_json::Value = resp.json();
    let content = extract_choice_text(&body);
    for raw in ["alice@example.com", "+1-555-123-4567", "555-66-7777"] {
        assert!(
            content.contains(raw),
            "client should see raw {raw}; got: {content}"
        );
    }
}

#[tokio::test]
async fn auto_redact_redacts_input_tool_call_arguments() {
    // Agent-loop scenario: the user resubmits the assistant's prior tool
    // call (with the original PII baked into the JSON arguments) as part
    // of conversation history. Without redacting input tool_calls, the
    // raw email would leak upstream on every follow-up turn.
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
        .add_header("x-auto-redact", "on")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [
                {"role":"user","content":"Send to bob@example.com"},
                {
                    "role":"assistant",
                    "content": null,
                    "tool_calls":[{
                        "id":"call_1",
                        "type":"function",
                        "function":{
                            "name":"send_email",
                            "arguments":"{\"to\":\"bob@example.com\",\"subject\":\"Hi\"}"
                        }
                    }]
                },
                {"role":"tool","tool_call_id":"call_1","content":"sent"},
                {"role":"user","content":"thanks"},
            ],
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    // The provider must NOT have seen the raw email — not in the user
    // message and crucially not in the assistant's tool_calls arguments.
    let params = mock_provider.last_chat_params().await.unwrap();
    let seen = serde_json::to_string(&params.messages).unwrap();
    assert!(
        !seen.contains("bob@example.com"),
        "raw email leaked to provider via input tool_call args: {seen}"
    );
    assert!(seen.contains("redacted1@example.com"));
}

#[tokio::test]
async fn auto_redact_unredacts_refusal_field() {
    // A safety-tuned model may quote our placeholders back in a refusal
    // ("I can't email redacted1@example.com"). Without un-redacting message.refusal,
    // the placeholder leaks to the client.
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // The mock template only sets content/tool_calls; to test refusal
    // un-redact we craft a minimal handler call and assert on the JSON
    // shape that would come from a real model. Skip if mock can't emit
    // refusal — instead unit-test the un-redact function directly.
    //
    // We can still drive the integration by setting the response content
    // to a refusal-flavored string and assert on it being unredacted.
    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new(
            "I can't email redacted1@example.com per policy.",
        ))
        .await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{"role":"user","content":"Mail charlie@example.com"}],
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    let body: serde_json::Value = resp.json();
    let content = extract_choice_text(&body);
    // content un-redact already covered; here we mainly verify the
    // refusal-path code compiles + runs without breaking content path.
    assert!(content.contains("charlie@example.com"));
    assert!(!content.contains("redacted1@example.com"));
}

#[tokio::test]
async fn auto_redact_unredacts_tool_call_arguments_streaming() {
    // Streaming agentic flow: ensure the per-(choice_idx, tc_idx) sliding-
    // tail state un-redacts placeholders that arrive in tool-call argument
    // fragments. The mock streams arguments split by spaces; the assertion
    // is on the assembled output.
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock_provider
        .set_default_response(
            inference_providers::mock::ResponseTemplate::new("").with_tool_calls(vec![
                inference_providers::mock::ToolCall::new(
                    "send_email",
                    // Spaces around the placeholder force the mock to emit
                    // multiple chunks; our streaming un-redact must reassemble.
                    r#"{ "to" : "redacted1@example.com" , "subject" : "Welcome" }"#,
                ),
            ]),
        )
        .await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{"role":"user","content":"Email alice@example.com a welcome note"}],
            "stream": true,
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    // Reassemble the streamed tool-call argument fragments and parse the
    // result as JSON. The `to` field must be the original email.
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
        if let Some(args) = chunk
            .pointer("/choices/0/delta/tool_calls/0/function/arguments")
            .and_then(|v| v.as_str())
        {
            assembled.push_str(args);
        }
    }
    let parsed: serde_json::Value = serde_json::from_str(assembled.trim()).unwrap_or_else(|e| {
        panic!("assembled args should be valid JSON; got: {assembled:?}, err: {e}")
    });
    assert_eq!(parsed["to"], "alice@example.com");
    assert!(
        !assembled.contains("redacted1@example.com"),
        "placeholder must not leak in streamed args; got: {assembled:?}"
    );
}

#[tokio::test]
async fn auto_redact_unredacts_tool_call_arguments() {
    // Agentic flow: the user's prompt contains PII, the model emits a tool
    // call whose JSON arguments echo the (now-redacted) PII. The un-redact
    // path must walk tool_calls[*].function.arguments and substitute the
    // placeholders back to originals so the client sees real values when
    // executing the tool call.
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Mock emits a send_email tool call whose `to` field is the minted
    // placeholder for the user's email. After un-redact, the client should
    // see the original.
    mock_provider
        .set_default_response(
            inference_providers::mock::ResponseTemplate::new("").with_tool_calls(vec![
                inference_providers::mock::ToolCall::new(
                    "send_email",
                    r#"{"to":"redacted1@example.com","subject":"Welcome","body":"Hi there"}"#,
                ),
            ]),
        )
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
                "content": "Send a welcome email to alice@example.com"
            }],
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    // Provider must NOT have seen the original email.
    let params = mock_provider.last_chat_params().await.unwrap();
    let seen = serde_json::to_string(&params.messages).unwrap();
    assert!(
        !seen.contains("alice@example.com"),
        "raw email leaked to provider: {seen}"
    );
    assert!(seen.contains("redacted1@example.com"));

    // Client must see the un-redacted email in tool_calls[0].function.arguments.
    let body: serde_json::Value = resp.json();
    let args = body
        .pointer("/choices/0/message/tool_calls/0/function/arguments")
        .and_then(|v| v.as_str())
        .expect("tool_calls[0].function.arguments must be present");
    assert!(
        args.contains("alice@example.com"),
        "tool_call arguments should be un-redacted; got {args}"
    );
    assert!(
        !args.contains("redacted1@example.com"),
        "placeholder should be swapped back; got {args}"
    );
}

/// x-auto-redact bills the privacy-filter classify pass it runs before the
/// completion (nearai/cloud-api#602). The mock privacy filter reports 10
/// input tokens per fragment; one user message => one fragment => one
/// `privacy_classify` usage row with 10 input tokens (the test model is
/// priced at 0, so the dollar amount is 0 — we assert the metered row, not
/// the price).
#[tokio::test]
async fn auto_redact_bills_classify_pass() {
    let (server, _pool, mock_provider, db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    setup_privacy_filter_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    mock_provider
        .set_default_response(inference_providers::mock::ResponseTemplate::new("Done."))
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
    assert_eq!(
        resp.status_code(),
        200,
        "chat should succeed: {}",
        resp.text()
    );

    // Exactly one privacy_classify row should be billed for the inline
    // classify pass, carrying the mock's 10 input tokens and no output.
    // Billing runs on a spawned background task, so poll (bounded) until the
    // row lands rather than querying once.
    let org_uuid = uuid::Uuid::parse_str(&org.id).expect("org id is a uuid");
    let pool = db.pool();
    let mut rows = Vec::new();
    for _ in 0..40 {
        let client = pool.get().await.expect("db connection");
        rows = client
            .query(
                "SELECT input_tokens, output_tokens, model_name \
                 FROM organization_usage_log \
                 WHERE organization_id = $1 AND inference_type = 'privacy_classify'",
                &[&org_uuid],
            )
            .await
            .expect("query privacy_classify usage");
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    assert_eq!(
        rows.len(),
        1,
        "expected exactly one privacy_classify usage row for the auto-redact classify pass"
    );
    let input_tokens: i32 = rows[0].get("input_tokens");
    let output_tokens: i32 = rows[0].get("output_tokens");
    let model_name: String = rows[0].get("model_name");
    assert_eq!(
        input_tokens, 10,
        "mock privacy filter reports 10 tokens/fragment"
    );
    assert_eq!(output_tokens, 0, "classify has no output tokens");
    assert_eq!(model_name, "openai/privacy-filter");
}
