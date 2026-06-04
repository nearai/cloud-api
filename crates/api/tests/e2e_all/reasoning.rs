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

use crate::common::*;
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
