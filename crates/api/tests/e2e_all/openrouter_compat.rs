//! Aggregator-compatibility behaviors reported against `z-ai/glm-5.3-flash`:
//!
//!   1. `reasoning_content` on a *previous* assistant message must reach the
//!      engine, otherwise a thinking model cannot continue its own chain of
//!      thought across tool calls (it silently re-reasons from scratch).
//!   2. A modality the catalog does not declare for the model (e.g. video) is
//!      a client error: a deterministic 400 before dispatch, never a retried
//!      502 after the engine chokes on the input.

use crate::common::*;
use inference_providers::mock::{RequestMatcher, ResponseTemplate};
use std::sync::Arc;

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

fn reasoning_repro(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "temperature": 0,
        "max_tokens": 64,
        "tools": [{"type": "function", "function": {"name": "get_time", "description": "Get the current time",
                   "parameters": {"type": "object", "properties": {}}}}],
        "messages": [
            {"role": "user", "content": "Call the get_time tool. While thinking, choose a secret word."},
            {"role": "assistant", "content": null,
             "reasoning_content": "The secret word is \"xylophone\".",
             "tool_calls": [{"id": "call_get_time_1", "type": "function",
                             "function": {"name": "get_time", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "call_get_time_1", "content": "12:00"}
        ]
    })
}

/// The prior assistant turn's `reasoning_content` is forwarded to the
/// provider verbatim, alongside its tool calls, for JSON and SSE requests.
#[tokio::test]
async fn test_prior_assistant_reasoning_content_reaches_provider() {
    let (server, mock, model, api_key) = setup().await;
    mock.when(RequestMatcher::Any)
        .respond_with(ResponseTemplate::new("SECRET: xylophone"))
        .await;

    for stream in [false, true] {
        let mut body = reasoning_repro(&model);
        body["stream"] = serde_json::json!(stream);
        let response = server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .await;
        assert_eq!(
            response.status_code(),
            200,
            "stream={stream}: {}",
            response.text()
        );

        let params = mock.last_chat_params().await.expect("provider was called");
        let assistant = params
            .messages
            .iter()
            .find(|m| m.role == inference_providers::MessageRole::Assistant)
            .expect("assistant history message");
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("The secret word is \"xylophone\"."),
            "stream={stream}: reasoning_content must survive to the provider"
        );
        assert_eq!(
            assistant.tool_calls.as_ref().map(|t| t.len()),
            Some(1),
            "stream={stream}: tool history intact"
        );
        // Other turns carry none (the field is only ever what the client sent).
        assert!(params
            .messages
            .iter()
            .filter(|m| m.role != inference_providers::MessageRole::Assistant)
            .all(|m| m.reasoning_content.is_none()));
    }
}

/// OpenRouter's dialect spells the field `reasoning`; accept it as an alias.
#[tokio::test]
async fn test_reasoning_alias_is_accepted() {
    let (server, mock, model, api_key) = setup().await;
    mock.when(RequestMatcher::Any)
        .respond_with(ResponseTemplate::new("ok"))
        .await;
    let mut body = reasoning_repro(&model);
    let assistant = body["messages"][1].as_object_mut().unwrap();
    // Move (not copy) the value: the canonical key must be absent, or serde
    // sees the alias as a duplicate of it.
    let reasoning = assistant.remove("reasoning_content").unwrap();
    assistant.insert("reasoning".to_string(), reasoning);
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let params = mock.last_chat_params().await.unwrap();
    let assistant = params
        .messages
        .iter()
        .find(|m| m.role == inference_providers::MessageRole::Assistant)
        .unwrap();
    assert_eq!(
        assistant.reasoning_content.as_deref(),
        Some("The secret word is \"xylophone\".")
    );
}

/// Register a model that declares text+image only.
async fn register_text_image_model(server: &axum_test::TestServer) -> String {
    let model_name = format!("e2e/or-compat-text-image-{}", uuid::Uuid::new_v4().simple());
    let mut batch = api::models::BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 1_000_000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2_000_000, "currency": "USD" },
            "modelDisplayName": "OR compat text+image",
            "modelDescription": "Synthetic text+image model for modality validation",
            "contextLength": 4_096,
            "verifiable": false,
            "isActive": true,
            "inputModalities": ["text", "image"],
            "outputModalities": ["text"]
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(server, batch, get_session_id()).await;
    model_name
}

/// A modality the catalog does not declare is refused up front with 400 and
/// the provider is never called; declared modalities still dispatch.
#[tokio::test]
async fn test_undeclared_input_modality_is_400_before_dispatch() {
    let (server, pool, _shared_mock, _db) = setup_test_server_with_pool().await;
    let model = register_text_image_model(&server).await;
    // The shared mock only serves the seeded fixtures; give the custom model
    // its own accept-all provider so a declared modality can actually dispatch.
    let mock = Arc::new(inference_providers::mock::MockProvider::new_accept_all());
    mock.set_default_response(ResponseTemplate::new("described"))
        .await;
    pool.register_provider(model.clone(), mock.clone()).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let video = serde_json::json!({
        "model": model,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "Describe this video."},
            {"type": "video_url", "video_url": {"url": "https://example.com/clip.mp4"}}
        ]}]
    });
    for stream in [false, true] {
        let mut body = video.clone();
        body["stream"] = serde_json::json!(stream);
        let response = server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .await;
        assert_eq!(
            response.status_code(),
            400,
            "stream={stream}: {}",
            response.text()
        );
        let json = response.json::<serde_json::Value>();
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert_eq!(json["error"]["param"], "messages");
        let message = json["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("does not support video input"),
            "{message}"
        );
        assert!(message.contains("text, image"), "{message}");
    }
    assert!(
        mock.last_chat_params().await.is_none(),
        "undeclared modality must not reach the provider"
    );

    // Audio in either spelling is refused the same way.
    let audio = serde_json::json!({
        "model": model,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": [
            {"type": "input_audio", "input_audio": {"data": "AAAA", "format": "wav"}}
        ]}]
    });
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&audio)
        .await;
    assert_eq!(response.status_code(), 400, "{}", response.text());
    assert!(response.text().contains("does not support audio input"));

    // Text + image is declared: dispatched normally.
    let image = serde_json::json!({
        "model": model,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "Describe this image."},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}}
        ]}]
    });
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&image)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(mock.last_chat_params().await.is_some());
}

/// A model without a catalog modality declaration keeps today's behavior:
/// nothing is rejected up front and the engine decides.
#[tokio::test]
async fn test_model_without_declared_modalities_is_not_gated() {
    let (server, mock, model, api_key) = setup().await;
    mock.when(RequestMatcher::Any)
        .respond_with(ResponseTemplate::new("ok"))
        .await;
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "max_tokens": 8,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "Describe this video."},
                {"type": "video_url", "video_url": {"url": "https://example.com/clip.mp4"}}
            ]}]
        }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
}

/// The engine's admission rejection (`--max-queued-requests` overflow) arrives
/// on a stream as a first SSE event `data: {"error": {..., "code": 503}}`.
/// After cloud-api's own backend fallback is exhausted it must surface as a
/// real HTTP 429 (`service_overloaded`, with `Retry-After`) before any SSE
/// bytes — never a 200 that fails mid-stream and never a 5xx, which an
/// aggregator would score as downtime rather than back-pressure.
#[tokio::test]
async fn test_streaming_queue_full_surfaces_as_429_before_sse() {
    let (server, mock, model, api_key) = setup().await;
    mock.set_stream_error_override(Some(inference_providers::CompletionError::HttpError {
        status_code: 503,
        message: "The request queue is full.".to_string(),
        is_external: false,
    }))
    .await;
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "max_tokens": 8
        }))
        .await;
    assert_eq!(response.status_code(), 429, "{}", response.text());
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json"),
        "the rejection must be a JSON error, not an SSE body"
    );
    let retry_after = response
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    assert!(retry_after.is_some_and(|s| s > 0), "Retry-After missing");
    let body = response.json::<serde_json::Value>();
    assert_eq!(body["error"]["type"], "service_overloaded");
}
