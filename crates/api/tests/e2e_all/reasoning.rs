//! Reasoning-content e2e tests (mocked backend).
//!
//! Ported from the live infra-tests `tests/test_reasoning.py`, which toggles
//! reasoning via `chat_template_kwargs` and inspects `reasoning_content` from
//! real models. With a mocked provider we cannot make the model *decide* to
//! reason, so we split the live test's intent into the two halves cloud-api is
//! actually responsible for:
//!
//!   1. when the provider emits `reasoning_content`, cloud-api surfaces it in
//!      the response (non-streaming and streaming), and
//!   2. the `chat_template_kwargs` toggle is forwarded to the provider intact
//!      (it rides in the `extra` passthrough map).
//!
//! The mock engine reports reasoning usage in SGLang's shape (a top-level
//! `usage.reasoning_tokens`), which lets the usage tests below check where
//! cloud-api exposes that count.

use crate::common::*;
use bytes::Bytes;
use inference_providers::mock::{RequestMatcher, ResponseTemplate};
use std::sync::Arc;

/// Provision a server (mocked provider), a registered model, a funded org and
/// an API key. Each test wires its own `respond_with` afterwards, since the
/// response template differs per case.
async fn setup() -> (
    axum_test::TestServer,
    Arc<inference_providers::mock::MockProvider>,
    String,
    String,
) {
    let (server, _pool, mock, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    (server, mock, model, api_key)
}

/// When the backend returns reasoning, cloud-api surfaces it on the
/// non-streaming chat completion.
#[tokio::test]
async fn test_reasoning_content_surfaced_non_streaming() {
    let (server, mock, model, api_key) = setup().await;

    mock.when(RequestMatcher::Any)
        .respond_with(
            ResponseTemplate::new("The answer is 42.")
                .with_reasoning("Let me think step by step about the question."),
        )
        .await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "What is the answer?"}],
            "max_tokens": 50,
            "stream": false,
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "expected 200, got: {}",
        response.text()
    );
    let body = response.json::<serde_json::Value>();
    let msg = &body["choices"][0]["message"];
    let reasoning = msg["reasoning_content"]
        .as_str()
        .or_else(|| msg["reasoning"].as_str());
    assert_eq!(
        reasoning,
        Some("Let me think step by step about the question."),
        "reasoning_content not surfaced in non-streaming response: {body}"
    );
}

/// Same, but for streaming: reasoning deltas appear in the SSE stream and
/// reassemble to the value the provider emitted. We parse the `data:` lines
/// rather than substring-matching the raw body, so the assertion can't pass on
/// an unrelated field or formatting.
#[tokio::test]
async fn test_reasoning_content_surfaced_streaming() {
    let (server, mock, model, api_key) = setup().await;

    mock.when(RequestMatcher::Any)
        .respond_with(
            ResponseTemplate::new("Final answer.").with_reasoning("Thinking about it carefully."),
        )
        .await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "What is the answer?"}],
            "max_tokens": 50,
            "stream": true,
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let body = response.text();

    // Reassemble reasoning from the streamed deltas.
    let mut reasoning = String::new();
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data.trim() == "[DONE]" {
            continue;
        }
        let Ok(chunk) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        if let Some(delta) = chunk.pointer("/choices/0/delta") {
            if let Some(rc) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                reasoning.push_str(rc);
            } else if let Some(r) = delta.get("reasoning").and_then(|v| v.as_str()) {
                reasoning.push_str(r);
            }
        }
    }
    assert!(
        reasoning.contains("Thinking about it carefully"),
        "reasoning not reassembled from streamed deltas (got {reasoning:?}): {body}"
    );
}

/// The `chat_template_kwargs` reasoning toggle must be forwarded to the
/// provider verbatim (it is not a first-class field, so it rides in `extra`).
#[tokio::test]
async fn test_chat_template_kwargs_forwarded() {
    let (server, mock, model, api_key) = setup().await;

    mock.when(RequestMatcher::Any)
        .respond_with(ResponseTemplate::new("ok"))
        .await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Say hi."}],
            "chat_template_kwargs": {"enable_thinking": false},
            "max_tokens": 10,
            "stream": false,
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "chat_template_kwargs should be accepted, got: {}",
        response.text()
    );
    let params = mock.last_chat_params().await.expect("provider was called");
    let kwargs = params
        .extra
        .get("chat_template_kwargs")
        .expect("chat_template_kwargs forwarded in `extra`");
    assert_eq!(
        kwargs.get("enable_thinking").and_then(|v| v.as_bool()),
        Some(false),
        "chat_template_kwargs.enable_thinking not preserved"
    );
}

const REASONING: &str = "Let me think step by step about the question.";

/// The mock engine reports one token per reasoning word.
fn reasoning_token_count() -> i64 {
    REASONING.split_whitespace().count() as i64
}

/// Parsed JSON payloads of every `data:` event except `[DONE]`.
fn sse_json_events(body: &str) -> Vec<serde_json::Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| data.trim() != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("SSE data should be JSON"))
        .collect()
}

/// Fetch the ECDSA signature stored for `chat_id` and check that it has the
/// expected `kind` and covers exactly the request and response bytes.
async fn assert_signature_covers(
    server: &axum_test::TestServer,
    api_key: &str,
    chat_id: &str,
    kind: &str,
    request_json: &str,
    response_body: &str,
) {
    let response = server
        .get(format!("/v1/signature/{chat_id}?signing_algo=ecdsa").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(
        response.status_code(),
        200,
        "signature should be available: {}",
        response.text()
    );
    let signature = response.json::<serde_json::Value>();
    assert_eq!(signature["signature_kind"], kind);
    assert_eq!(
        signature["text"],
        format!(
            "{}:{}",
            compute_sha256(request_json),
            compute_sha256(response_body)
        ),
        "the {kind} signature must cover the exact request and response bytes"
    );
}

/// Issue #1015: OpenAI-compatible clients (e.g. the Vercel AI SDK) read the
/// reasoning count only from `usage.completion_tokens_details.reasoning_tokens`
/// on the final usage chunk. SGLang reports it top-level, so the final usage
/// chunk carries it in both places. The gateway synthesizes that chunk, so its
/// signature covers the exact bytes the client received.
#[tokio::test]
async fn test_streaming_final_usage_chunk_reports_reasoning_tokens() {
    let (server, mock, model, api_key) = setup().await;

    mock.when(RequestMatcher::Any)
        .respond_with(ResponseTemplate::new("The answer is 42.").with_reasoning(REASONING))
        .await;

    let request_json = serde_json::to_string(&serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "What is the answer?"}],
        "max_tokens": 50,
        "stream": true,
        "stream_options": {"include_usage": true},
    }))
    .expect("request should serialize");
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .content_type("application/json")
        .bytes(Bytes::from(request_json.clone()))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.text();

    let usage_events: Vec<serde_json::Value> = sse_json_events(&body)
        .into_iter()
        .filter(|event| event["usage"].is_object())
        .collect();
    assert_eq!(
        usage_events.len(),
        1,
        "expected exactly one usage chunk: {body}"
    );
    let final_chunk = &usage_events[0];
    assert!(
        final_chunk["choices"]
            .as_array()
            .is_some_and(|choices| choices.is_empty()),
        "usage must arrive on the final choices:[] chunk: {final_chunk}"
    );
    let usage = &final_chunk["usage"];
    let expected = reasoning_token_count();
    assert_eq!(
        usage["completion_tokens_details"]["reasoning_tokens"], expected,
        "standard reasoning count missing from the final usage chunk: {usage}"
    );
    assert_eq!(
        usage["reasoning_tokens"], expected,
        "legacy top-level reasoning count should be kept: {usage}"
    );
    assert!(
        usage["completion_tokens"]
            .as_i64()
            .is_some_and(|completion| completion >= expected),
        "reasoning is a subset of completion_tokens: {usage}"
    );

    let chat_id = final_chunk["id"].as_str().expect("chunk should have an id");
    assert_signature_covers(&server, &api_key, chat_id, "gateway", &request_json, &body).await;
}

/// Non-streaming self-hosted bodies are the model TEE's signed bytes. cloud-api
/// returns them unchanged, so the usage keeps the engine's shape here and the
/// provider signature still verifies against the body the client received.
#[tokio::test]
async fn test_non_streaming_self_hosted_usage_is_passed_through_unchanged() {
    let (server, mock, model, api_key) = setup().await;

    mock.when(RequestMatcher::Any)
        .respond_with(ResponseTemplate::new("The answer is 42.").with_reasoning(REASONING))
        .await;

    let request_json = serde_json::to_string(&serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "What is the answer?"}],
        "max_tokens": 50,
        "stream": false,
    }))
    .expect("request should serialize");
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .content_type("application/json")
        .bytes(Bytes::from(request_json.clone()))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body_text = response.text();
    let body: serde_json::Value =
        serde_json::from_str(&body_text).expect("completion should be JSON");

    assert_eq!(body["usage"]["reasoning_tokens"], reasoning_token_count());
    assert!(
        body["usage"].get("completion_tokens_details").is_none(),
        "the provider-signed body must not be rewritten: {body}"
    );

    let chat_id = body["id"].as_str().expect("completion should have an id");
    // Non-streaming provider signatures are collected asynchronously.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !mock.unpinned_chat_ids().iter().any(|id| id == chat_id) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("provider signature should be stored");
    assert_signature_covers(
        &server,
        &api_key,
        chat_id,
        "provider_tee",
        &request_json,
        &body_text,
    )
    .await;
}

/// Auto-redact re-serializes the non-streaming body (the gateway signs it
/// instead of the provider), so that body also gets the standard field.
#[tokio::test]
async fn test_auto_redact_non_streaming_reports_reasoning_tokens() {
    let (server, mock, model, api_key) = setup().await;
    setup_privacy_filter_model(&server).await;

    mock.when(RequestMatcher::Any)
        .respond_with(
            ResponseTemplate::new("I'll email redacted1@example.com shortly.")
                .with_reasoning(REASONING),
        )
        .await;

    let request_json = serde_json::to_string(&serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "Please reach out to alice@example.com"}],
        "stream": false,
    }))
    .expect("request should serialize");
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .content_type("application/json")
        .bytes(Bytes::from(request_json.clone()))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body_text = response.text();
    let body: serde_json::Value =
        serde_json::from_str(&body_text).expect("completion should be JSON");
    assert!(
        body["choices"][0]["message"]["content"]
            .as_str()
            .is_some_and(|content| content.contains("alice@example.com")),
        "auto-redact should have restored the original PII: {body}"
    );

    let expected = reasoning_token_count();
    assert_eq!(
        body["usage"]["completion_tokens_details"]["reasoning_tokens"],
        expected
    );
    assert_eq!(body["usage"]["reasoning_tokens"], expected);

    let chat_id = body["id"].as_str().expect("completion should have an id");
    assert_signature_covers(
        &server,
        &api_key,
        chat_id,
        "gateway",
        &request_json,
        &body_text,
    )
    .await;
}
