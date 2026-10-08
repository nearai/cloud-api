//! Tinfoil wire format -> gateway (OpenAI) shape.
//!
//! Client-visible bytes are built by ALLOWLIST (same approach as the Chutes
//! provider): Tinfoil's vLLM-based router emits serving internals (`p` padding,
//! `token_ids`, `prompt_token_ids`, `prompt_text` which echoes the prompt,
//! `stop_reason`, `routed_experts`, `kv_transfer_params`, `metrics`,
//! `prompt_logprobs`, ...). Anything not on a list below is dropped, and an
//! event that cannot be sanitized is an error, never forwarded as-is.

use futures_util::StreamExt;
use serde_json::{Map, Value};

use crate::attested::openai_wire::{gate_stream_usage, retain_allowed, sanitize_response_object};
use crate::{
    ChatCompletionChunk, ChatCompletionResponse, CompletionError, SSEEvent, StreamChunk,
    StreamingResult,
};

const USAGE: &[&str] = &[
    "prompt_tokens",
    "completion_tokens",
    "total_tokens",
    "prompt_tokens_details",
    "completion_tokens_details",
];

fn retain_only(obj: &mut Map<String, Value>, key: &str, allowed: &[&str]) {
    match obj.get_mut(key) {
        Some(Value::Object(inner)) => retain_allowed(inner, allowed),
        // `null` details carry nothing.
        Some(_) => {
            obj.remove(key);
        }
        None => {}
    }
}

/// Sanitize one response/chunk object in place: slug -> canonical `model`,
/// Tinfoil's `reasoning` -> our `reasoning_content`, the shared OpenAI
/// allowlists, then Tinfoil's tighter `usage` sub-filtering.
fn sanitize(obj: &mut Map<String, Value>, canonical: &str) {
    obj.insert("model".to_string(), Value::String(canonical.to_string()));
    if let Some(choices) = obj.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices {
            for inner in ["message", "delta"] {
                let Some(m) = choice
                    .as_object_mut()
                    .and_then(|c| c.get_mut(inner))
                    .and_then(Value::as_object_mut)
                else {
                    continue;
                };
                if let Some(r) = m.remove("reasoning") {
                    if r.is_string()
                        && !matches!(m.get("reasoning_content"), Some(Value::String(_)))
                    {
                        m.insert("reasoning_content".to_string(), r);
                    }
                }
            }
        }
    }
    // Tinfoil emits no extras of its own: nothing beyond the common shape.
    sanitize_response_object(obj, &[], &[]);
    if let Some(Value::Object(usage)) = obj.get_mut("usage") {
        retain_allowed(usage, USAGE);
        retain_only(usage, "prompt_tokens_details", &["cached_tokens"]);
        retain_only(usage, "completion_tokens_details", &["reasoning_tokens"]);
    }
}

fn invalid(what: &str) -> CompletionError {
    CompletionError::InvalidResponse(format!("Tinfoil {what} could not be mapped"))
}

/// Map a non-stream body. Returns the client-visible bytes and the parsed response.
pub(super) fn map_response(
    body: &[u8],
    canonical: &str,
) -> Result<(Vec<u8>, ChatCompletionResponse), CompletionError> {
    let mut v: Value = serde_json::from_slice(body).map_err(|_| invalid("response"))?;
    let obj = v.as_object_mut().ok_or_else(|| invalid("response"))?;
    sanitize(obj, canonical);
    let raw = serde_json::to_vec(&v).map_err(|_| invalid("response"))?;
    let parsed: ChatCompletionResponse =
        serde_json::from_slice(&raw).map_err(|_| invalid("response"))?;
    Ok((raw, parsed))
}

/// Map one SSE event. Control lines pass through. The parsed chunk keeps `usage`
/// (billing reads it); the client-visible bytes follow the usage gate, with the
/// final usage-only chunk suppressed unless the client asked for usage.
pub(super) fn map_event(
    mut ev: SSEEvent,
    canonical: &str,
    client_wants_usage: bool,
) -> Result<SSEEvent, CompletionError> {
    if ev.chunk.is_none() {
        return Ok(ev);
    }
    let s = std::str::from_utf8(&ev.raw_bytes).map_err(|_| invalid("event"))?;
    let data = s
        .trim()
        .strip_prefix("data:")
        .map(str::trim)
        .unwrap_or(s.trim());
    let mut v: Value = serde_json::from_str(data).map_err(|_| invalid("event"))?;
    let obj = v.as_object_mut().ok_or_else(|| invalid("event"))?;
    sanitize(obj, canonical);

    let chunk: ChatCompletionChunk =
        serde_json::from_value(v.clone()).map_err(|_| invalid("event"))?;
    ev.chunk = Some(StreamChunk::Chat(chunk));

    let obj = v.as_object_mut().ok_or_else(|| invalid("event"))?;
    if gate_stream_usage(obj, client_wants_usage) {
        ev.raw_bytes = bytes::Bytes::new();
        return Ok(ev);
    }
    let json = serde_json::to_string(&v).map_err(|_| invalid("event"))?;
    ev.raw_bytes = bytes::Bytes::from(format!("data: {json}\n\n"));
    Ok(ev)
}

pub(super) fn map_stream(
    inner: StreamingResult,
    canonical: String,
    client_wants_usage: bool,
) -> StreamingResult {
    Box::pin(
        inner.map(move |item| item.and_then(|ev| map_event(ev, &canonical, client_wants_usage))),
    )
}
