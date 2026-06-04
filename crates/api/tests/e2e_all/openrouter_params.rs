//! OpenRouter / OpenAI parameter-conformance e2e tests (mocked backend).
//!
//! Ported from the live infra-tests suite `tests/test_openrouter_params.py`,
//! which hits `cloud-api.near.ai`. That suite asserts behavioural outcomes
//! against real models; here we assert the equivalent *cloud-api contract*
//! against the self-hosted (vLLM) path with the mocked provider:
//!
//!   1. the parameter is **accepted** (HTTP 200, never 400/422), and
//!   2. it is **forwarded** to the inference provider — either as a typed field
//!      on `ChatCompletionParams` or, for the long tail of sampling knobs, in
//!      the `extra` passthrough map.
//!
//! Whether the model actually *honours* a parameter (e.g. `stop` truncating
//! output, `logprobs` populating the response) is the backend's job and stays
//! covered by the live infra-tests — the mock deliberately ignores these. The
//! bugs these tests guard (nearai/cloud-api #695/#696/#697/#619/#668) all
//! manifest as cloud-api *rejecting or dropping* a parameter, which a mocked
//! backend reproduces faithfully.
//!
//! NOTE on where a param lands: `ChatCompletionParams` has typed `seed`,
//! `logprobs` and `top_logprobs` fields, but the service-layer conversion
//! (`crates/services/src/completions/mod.rs`, `create_chat_completion[_stream]`)
//! hardcodes those to `None` and forwards the originals through the `extra`
//! passthrough map. So the assertions below correctly look for `seed` /
//! `logprobs` / `top_logprobs` in `params.extra`, *not* in the typed fields —
//! that is what the live request actually carries to the provider.

use crate::common::*;
use inference_providers::mock::{RequestMatcher, ResponseTemplate};
use inference_providers::ToolChoice;
use std::sync::Arc;

/// Provision a server (mocked provider), a registered model, a funded org and
/// an API key. Returns the pieces every test below needs.
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
    mock.when(RequestMatcher::Any)
        .respond_with(ResponseTemplate::new("ok"))
        .await;
    (server, mock, model, api_key)
}

/// The weather tool shared by the tool_choice cases (mirrors the infra suite).
fn weather_tool() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get current weather for a city.",
            "parameters": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }
        }
    })
}

// ── stop (nearai/cloud-api PR #695) ─────────────────────────────────────────

/// Array form of `stop` is accepted and forwarded to the provider verbatim.
#[tokio::test]
async fn test_stop_array_accepted_and_forwarded() {
    let (server, mock, model, api_key) = setup().await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Count from 1 to 20."}],
            "stop": ["5"],
            "max_tokens": 40,
            "stream": false,
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "stop array should be accepted, got: {}",
        response.text()
    );
    let params = mock.last_chat_params().await.expect("provider was called");
    assert_eq!(
        params.stop,
        Some(vec!["5".to_string()]),
        "stop array not forwarded to provider"
    );
}

/// OpenAI spec allows `stop` as a single bare string; OpenRouter sends this
/// form. cloud-api must accept it and normalise it to a one-element list before
/// forwarding (regression guard for the 422 fixed by PR #695).
#[tokio::test]
async fn test_stop_string_accepted_and_normalized() {
    let (server, mock, model, api_key) = setup().await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Count from 1 to 20."}],
            "stop": "5",
            "max_tokens": 40,
            "stream": false,
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "bare-string stop should be accepted (cloud-api#695), got: {}",
        response.text()
    );
    let params = mock.last_chat_params().await.expect("provider was called");
    assert_eq!(
        params.stop,
        Some(vec!["5".to_string()]),
        "bare-string stop should normalise to a single-element list"
    );
}

// ── temperature (nearai/cloud-api #696) ─────────────────────────────────────

/// A non-default temperature must be accepted and forwarded (not rejected the
/// way opus-4-7 did in #696).
#[tokio::test]
async fn test_temperature_nondefault_accepted_and_forwarded() {
    let (server, mock, model, api_key) = setup().await;

    for temp in [0.0_f64, 0.7_f64] {
        let response = server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "Say hi."}],
                "temperature": temp,
                "max_tokens": 10,
                "stream": false,
            }))
            .await;

        assert_eq!(
            response.status_code(),
            200,
            "temperature={temp} should be accepted, got: {}",
            response.text()
        );
        let params = mock.last_chat_params().await.expect("provider was called");
        let forwarded = params.temperature.expect("temperature forwarded") as f64;
        assert!(
            (forwarded - temp).abs() < 1e-6,
            "temperature {temp} not forwarded (got {forwarded})"
        );
    }
}

// ── optional sampling extras (nearai/cloud-api #697) ────────────────────────

/// Sampling knobs that are not first-class OpenAI fields (top_k, top_a,
/// repetition_penalty, min_p) must be accepted and ride through to the provider
/// in the `extra` passthrough map rather than being rejected (#697).
#[tokio::test]
async fn test_sampling_extras_accepted_and_forwarded() {
    let (server, mock, model, api_key) = setup().await;

    let extras = [
        ("top_k", serde_json::json!(5)),
        ("top_a", serde_json::json!(0.5)),
        ("repetition_penalty", serde_json::json!(1.3)),
        ("min_p", serde_json::json!(0.5)),
    ];

    for (param, value) in extras {
        // Build the body with the dynamic key inserted explicitly (rather than
        // relying on `json!`'s identifier-key handling) so the parametrisation
        // is unambiguous.
        let mut body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Say hi."}],
            "max_tokens": 10,
            "stream": false,
        });
        body.as_object_mut()
            .unwrap()
            .insert(param.to_string(), value.clone());

        let response = server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .await;

        assert_eq!(
            response.status_code(),
            200,
            "sampling extra {param} should be accepted, got: {}",
            response.text()
        );
        let params = mock.last_chat_params().await.expect("provider was called");
        assert_eq!(
            params.extra.get(param),
            Some(&value),
            "sampling extra {param} not forwarded in `extra`"
        );
    }
}

// ── seed ────────────────────────────────────────────────────────────────────

/// `seed` must never error and must be forwarded (it rides in `extra`).
#[tokio::test]
async fn test_seed_accepted_and_forwarded() {
    let (server, mock, model, api_key) = setup().await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Say hi."}],
            "seed": 12345,
            "temperature": 1.0,
            "max_tokens": 10,
            "stream": false,
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "seed should be accepted, got: {}",
        response.text()
    );
    let params = mock.last_chat_params().await.expect("provider was called");
    assert_eq!(
        params.extra.get("seed").and_then(|v| v.as_i64()),
        Some(12345),
        "seed not forwarded in `extra`"
    );
}

// ── logprobs / top_logprobs ─────────────────────────────────────────────────

/// `logprobs` + `top_logprobs` must be accepted and forwarded to the provider.
#[tokio::test]
async fn test_logprobs_accepted_and_forwarded() {
    let (server, mock, model, api_key) = setup().await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Say hi."}],
            "logprobs": true,
            "top_logprobs": 5,
            "max_tokens": 10,
            "stream": false,
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "logprobs should be accepted, got: {}",
        response.text()
    );
    let params = mock.last_chat_params().await.expect("provider was called");
    assert_eq!(
        params.extra.get("logprobs").and_then(|v| v.as_bool()),
        Some(true),
        "logprobs not forwarded in `extra`"
    );
    assert_eq!(
        params.extra.get("top_logprobs").and_then(|v| v.as_i64()),
        Some(5),
        "top_logprobs not forwarded in `extra`"
    );
}

// ── tools / tool_choice (nearai/cloud-api #619) ─────────────────────────────

/// `tool_choice: "required"` together with a tool definition is accepted and
/// both are lifted into the typed provider params.
#[tokio::test]
async fn test_tool_choice_required_accepted_and_forwarded() {
    let (server, mock, model, api_key) = setup().await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "What is the weather in Paris?"}],
            "tools": [weather_tool()],
            "tool_choice": "required",
            "max_tokens": 200,
            "stream": false,
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "tool_choice=required should be accepted, got: {}",
        response.text()
    );
    let params = mock.last_chat_params().await.expect("provider was called");
    assert!(
        matches!(params.tool_choice, Some(ToolChoice::String(ref s)) if s == "required"),
        "tool_choice=required not forwarded (got {:?})",
        params.tool_choice
    );
    assert!(
        params
            .tools
            .as_ref()
            .map(|t| t.iter().any(|d| d.function.name == "get_weather"))
            .unwrap_or(false),
        "tools not forwarded alongside tool_choice"
    );
}

/// `tool_choice: "none"` must be accepted, and cloud-api must STRIP the tools
/// from the upstream request so the backend cannot emit a tool call regardless
/// of whether it honors `tool_choice` (#619: vLLM-served models ignore "none").
/// We assert the tools array is gone while `tool_choice: "none"` is still
/// forwarded (harmless, and preserves intent for backends that do honor it).
#[tokio::test]
async fn test_tool_choice_none_strips_tools() {
    let (server, mock, model, api_key) = setup().await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "What is the weather in Paris?"}],
            "tools": [weather_tool()],
            "tool_choice": "none",
            "max_tokens": 200,
            "stream": false,
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "tool_choice=none should be accepted, got: {}",
        response.text()
    );
    let params = mock.last_chat_params().await.expect("provider was called");
    assert!(
        params.tools.is_none(),
        "tool_choice=none must strip the tools array (got {:?})",
        params.tools
    );
    // The tools must not have leaked into the extra passthrough map either.
    assert!(
        !params.extra.contains_key("tools"),
        "tool_choice=none must also strip `tools` from extra"
    );
    assert!(
        matches!(params.tool_choice, Some(ToolChoice::String(ref s)) if s == "none"),
        "tool_choice=none itself should still be forwarded (got {:?})",
        params.tool_choice
    );
}

// ── response_format json_schema (nearai/cloud-api #668) ─────────────────────

/// A `response_format: { type: json_schema, ... }` must be accepted and
/// forwarded to the provider (it rides in `extra`). #668 tracks passthrough
/// providers that ignore it; cloud-api must not reject or drop it.
#[tokio::test]
async fn test_json_schema_response_format_accepted_and_forwarded() {
    let (server, mock, model, api_key) = setup().await;

    let response_format = serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "person",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "age": {"type": "integer"}
                },
                "required": ["name", "age"],
                "additionalProperties": false
            }
        }
    });

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Generate a fictional person."}],
            "response_format": response_format,
            "max_tokens": 300,
            "stream": false,
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "json_schema response_format should be accepted, got: {}",
        response.text()
    );
    let params = mock.last_chat_params().await.expect("provider was called");
    let forwarded = params
        .extra
        .get("response_format")
        .expect("response_format forwarded in `extra`");
    assert_eq!(
        forwarded.get("type").and_then(|v| v.as_str()),
        Some("json_schema"),
        "response_format.type not preserved on passthrough"
    );
}
