//! OpenAI-shaped wire handling shared by the attested third-party providers
//! (Chutes, Tinfoil): the outbound request body, and the ALLOWLIST that decides
//! which fields of an upstream response reach the client.
//!
//! The allow-lists are the common OpenAI chat shape. A provider layers its own
//! extra keys on top (`extra_top` / `extra_message` of [`sanitize_response_object`])
//! and any provider-specific rewriting (e.g. Tinfoil's `reasoning` ->
//! `reasoning_content`) before calling it. Everything not listed is dropped.

use serde_json::{json, Value};

use crate::attested::nearai::placement_headers as ph;
use crate::ChatCompletionParams;

/// Internal `extra` keys that must never reach Chutes (a third party): the
/// tracing identifiers and the client-facing-E2EE markers. `ChatCompletionParams`
/// flattens `extra` into the top-level body, so these would otherwise leak.
pub(crate) const INTERNAL_KEYS: &[&str] = {
    use crate::attested::nearai::{encryption_headers as eh, tracing_headers as th};
    &[
        th::REQUEST_ID,
        th::ORG_ID,
        th::WORKSPACE_ID,
        eh::SIGNING_ALGO,
        eh::CLIENT_PUB_KEY,
        eh::MODEL_PUB_KEY,
        eh::ENCRYPTION_VERSION,
        eh::ENCRYPT_ALL_FIELDS,
    ]
};

/// Standard OpenAI chat-response / chunk fields kept at the TOP LEVEL. This is an
/// ALLOWLIST (#780 -> #781 follow-up): model-specific serving internals vary too
/// much to denylist reliably (e.g. kimi-k2.6 emits `templated_prompt`,
/// `prompt_token_ids`, `prompt_logprobs`, `kv_transfer_params`, ... alongside
/// `prompt_sha256`/`template_sha256`/`metadata`; Tinfoil's vLLM router emits `p`
/// padding, `prompt_text`, ...), so anything NOT on this list is dropped.
///
/// Includes `system_fingerprint` and `service_tier`, which ARE standard (do NOT
/// confuse them with serving internals). Providers add their own deliberately
/// surfaced extras via `extra_top`.
const TOP_LEVEL_FIELDS: &[&str] = &[
    "id",
    "object",
    "created",
    "model",
    "choices",
    "usage",
    "system_fingerprint",
    "service_tier",
];

/// Standard OpenAI fields kept on each element of `choices` (both the non-stream
/// `message`-bearing shape and the stream `delta`-bearing shape). `message` and
/// `delta` are recursed into via [`MESSAGE_FIELDS`].
const CHOICE_FIELDS: &[&str] = &["index", "delta", "message", "finish_reason", "logprobs"];

/// Standard OpenAI fields kept on a `choices[].message` (non-stream) or
/// `choices[].delta` (stream). Drops model-specific message internals (e.g. a
/// delta-nested `matched_stop`/`token_ids`).
const MESSAGE_FIELDS: &[&str] = &[
    "role",
    "content",
    "reasoning_content",
    "tool_calls",
    "function_call",
    "refusal",
    "annotations",
    "name",
    "tool_call_id",
];

/// Drop from `obj` every key that is NOT on `allowed`, in place.
pub(crate) fn retain_allowed(obj: &mut serde_json::Map<String, Value>, allowed: &[&str]) {
    obj.retain(|k, _| allowed.contains(&k.as_str()));
}

// Nested OpenAI schemas. Every retained container is allow-listed recursively so
// an unknown key (e.g. a vLLM `prompt_text` echo) cannot ride inside a container
// whose parent key is standard. Opaque content leaves (`content`, `reasoning*`,
// `function.arguments`, `custom.input`, `logprobs[].token`) are kept as-is.
const TOOL_CALL_FIELDS: &[&str] = &["index", "id", "type", "function", "custom"];
const TOOL_FUNCTION_FIELDS: &[&str] = &["name", "arguments"];
const TOOL_CUSTOM_FIELDS: &[&str] = &["name", "input"];
const ANNOTATION_FIELDS: &[&str] = &["type", "url_citation"];
const URL_CITATION_FIELDS: &[&str] = &["start_index", "end_index", "url", "title"];
const LOGPROBS_FIELDS: &[&str] = &["content", "refusal"];
const LOGPROB_ENTRY_FIELDS: &[&str] = &["token", "logprob", "bytes", "top_logprobs"];
const TOP_LOGPROB_FIELDS: &[&str] = &["token", "logprob", "bytes"];
const USAGE_DETAIL_PROMPT: &[&str] = &["cached_tokens", "audio_tokens"];
const USAGE_DETAIL_COMPLETION: &[&str] = &[
    "reasoning_tokens",
    "audio_tokens",
    "accepted_prediction_tokens",
    "rejected_prediction_tokens",
];

/// Allow-list the object stored at `obj[key]`. Scalars (null, string, number,
/// bool) cannot carry a nested key and are left alone; an array in an object
/// slot is not a valid shape and is removed.
fn sanitize_child(
    obj: &mut serde_json::Map<String, Value>,
    key: &str,
    allowed: &[&str],
    nested: fn(&mut serde_json::Map<String, Value>),
) {
    match obj.get_mut(key) {
        Some(Value::Object(inner)) => {
            retain_allowed(inner, allowed);
            nested(inner);
        }
        Some(Value::Array(_)) => {
            obj.remove(key);
        }
        Some(_) | None => {}
    }
}

/// Allow-list every element of the array at `obj[key]` (object elements only;
/// scalar elements are left alone). An object in an array slot is not a valid
/// shape and is removed; scalars are left alone.
fn sanitize_array(
    obj: &mut serde_json::Map<String, Value>,
    key: &str,
    allowed: &[&str],
    nested: fn(&mut serde_json::Map<String, Value>),
) {
    match obj.get_mut(key) {
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(o) = item.as_object_mut() {
                    retain_allowed(o, allowed);
                    nested(o);
                }
            }
        }
        Some(Value::Object(_)) => {
            obj.remove(key);
        }
        Some(_) | None => {}
    }
}

fn no_nested(_: &mut serde_json::Map<String, Value>) {}

fn sanitize_tool_call(tc: &mut serde_json::Map<String, Value>) {
    sanitize_child(tc, "function", TOOL_FUNCTION_FIELDS, no_nested);
    sanitize_child(tc, "custom", TOOL_CUSTOM_FIELDS, no_nested);
}

fn sanitize_annotation(a: &mut serde_json::Map<String, Value>) {
    sanitize_child(a, "url_citation", URL_CITATION_FIELDS, no_nested);
}

fn sanitize_logprob_entry(e: &mut serde_json::Map<String, Value>) {
    sanitize_array(e, "top_logprobs", TOP_LOGPROB_FIELDS, no_nested);
}

fn sanitize_logprobs(l: &mut serde_json::Map<String, Value>) {
    sanitize_array(l, "content", LOGPROB_ENTRY_FIELDS, sanitize_logprob_entry);
    sanitize_array(l, "refusal", LOGPROB_ENTRY_FIELDS, sanitize_logprob_entry);
}

/// `choices[].message` / `choices[].delta`: top-level keys by allow-list, then
/// each retained container recursively. Per-field choices: `audio` is not on
/// [`MESSAGE_FIELDS`] (dropped; we neither request nor bill audio output);
/// `refusal` is a string or null, a container there is dropped; `annotations` keep
/// only the standard `url_citation` shape; `content` and `reasoning*` are the
/// completion itself and pass through untouched.
fn sanitize_message(m: &mut serde_json::Map<String, Value>, extra_message: &[&str]) {
    m.retain(|k, _| MESSAGE_FIELDS.contains(&k.as_str()) || extra_message.contains(&k.as_str()));
    sanitize_array(m, "tool_calls", TOOL_CALL_FIELDS, sanitize_tool_call);
    sanitize_child(m, "function_call", TOOL_FUNCTION_FIELDS, no_nested);
    sanitize_array(m, "annotations", ANNOTATION_FIELDS, sanitize_annotation);
    if m.get("refusal")
        .is_some_and(|r| r.is_object() || r.is_array())
    {
        m.remove("refusal");
    }
}

/// `usage`: the standard counters and the two nested detail objects, each
/// allow-listed. Unknown numeric keys are kept (a stray counter is harmless and
/// dropping one could hide billing data); anything else unknown (strings such as
/// a `prompt_text` echo, containers) is dropped.
fn sanitize_usage(u: &mut serde_json::Map<String, Value>) {
    u.retain(|k, v| {
        v.is_number() || k == "prompt_tokens_details" || k == "completion_tokens_details"
    });
    sanitize_child(u, "prompt_tokens_details", USAGE_DETAIL_PROMPT, no_nested);
    sanitize_child(
        u,
        "completion_tokens_details",
        USAGE_DETAIL_COMPLETION,
        no_nested,
    );
}

/// Sanitize a response/chunk object to the standard OpenAI shape by ALLOWLIST, in
/// place: the top level keeps [`TOP_LEVEL_FIELDS`] plus `extra_top`, each
/// `choices[]` element keeps [`CHOICE_FIELDS`], and each `choices[].message` /
/// `choices[].delta` keeps [`MESSAGE_FIELDS`] plus `extra_message`. Retained
/// containers (`tool_calls`, `function_call`, `annotations`, `logprobs`, `usage`
/// details) are allow-listed recursively. Everything else, known internals and
/// unknown future ones alike, is dropped.
///
/// Conservative under-strip: when in doubt a field is added to the allowlist
/// (keeping a stray field is safer than dropping a legitimate one), but the known
/// serving internals are deliberately absent and therefore dropped. `extra_top`
/// / `extra_message` values are provider-owned and not inspected.
pub(crate) fn sanitize_response_object(
    obj: &mut serde_json::Map<String, Value>,
    extra_top: &[&str],
    extra_message: &[&str],
) {
    obj.retain(|k, _| TOP_LEVEL_FIELDS.contains(&k.as_str()) || extra_top.contains(&k.as_str()));
    if let Some(choices) = obj.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices {
            if let Some(choice_obj) = choice.as_object_mut() {
                retain_allowed(choice_obj, CHOICE_FIELDS);
                sanitize_child(choice_obj, "logprobs", LOGPROBS_FIELDS, sanitize_logprobs);
                // Recurse into the message (non-stream) and delta (stream) shapes.
                for inner in ["message", "delta"] {
                    if let Some(m) = choice_obj.get_mut(inner).and_then(Value::as_object_mut) {
                        sanitize_message(m, extra_message);
                    }
                }
            }
        }
    }
    if let Some(usage) = obj.get_mut("usage").and_then(Value::as_object_mut) {
        sanitize_usage(usage);
    }
}

/// Gate the `usage` field of a streamed chunk object to the OpenAI spec (#781 L1),
/// in place. Per spec, intermediate chunks carry `usage: null`; `usage` appears only
/// on the FINAL chunk, and only when the request set `stream_options.include_usage`.
/// We force `include_usage: true` upstream so billing always sees usage (see
/// [`request_body`]), so this gates only what reaches the CLIENT:
///
/// - a chunk with a non-empty `choices` array is an INTERMEDIATE content chunk → its
///   `usage` is always stripped (covers vLLM's `continuous_usage_stats`, which would
///   otherwise populate `usage` on every chunk);
/// - a chunk with an empty `choices` array is the FINAL usage-only chunk → its
///   `usage` is kept iff `include_usage` was requested, else stripped.
///
/// Returns `true` when the chunk was the FINAL usage-only chunk and the client did
/// NOT request usage: OpenAI emits no final usage chunk at all in that case, so the
/// caller must suppress the whole chunk from the client stream rather than forward
/// a gutted `choices: []` husk (strict SDK parsers reject it, and cost-tracking
/// clients read it as zero usage) — see the Chutes `rewrite_sse_event_model`.
///
/// NOTE: this gates only `raw_bytes` (the bytes the passthrough route forwards to the
/// client). The parsed `chunk.usage` is left intact so `InterceptStream` can still
/// read it for billing/limits — see the Chutes `rewrite_sse_event_model`.
pub(crate) fn gate_stream_usage(
    obj: &mut serde_json::Map<String, Value>,
    include_usage: bool,
) -> bool {
    if !obj.contains_key("usage") {
        return false;
    }
    let is_final = obj
        .get("choices")
        .and_then(Value::as_array)
        .is_none_or(|c| c.is_empty());
    if !is_final || !include_usage {
        obj.remove("usage");
    }
    is_final && !include_usage
}

/// An OpenAI request body (as JSON) with `model` pinned, `stream` set, and all
/// internal/tracing/E2EE-marker keys stripped (never sent to the third party).
pub(crate) fn request_body(
    model: &str,
    params: &ChatCompletionParams,
    stream: bool,
) -> Result<Value, String> {
    let mut v = serde_json::to_value(params).map_err(|e| format!("serialize params: {e}"))?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("model".to_string(), json!(model));
        obj.insert("stream".to_string(), json!(stream));
        if stream {
            // Force usage onto the final stream chunk so streamed tokens are
            // billed and counted against org limits (the OpenAI-compatible
            // default omits it, and our SSE adapter drops Chutes' outer
            // usage-only events). Matches every other provider.
            //
            // Merge into any client-supplied `stream_options` (e.g.
            // `continuous_usage_stats`) rather than clobbering the whole object —
            // we only need to *guarantee* `include_usage`.
            match obj.get_mut("stream_options").and_then(Value::as_object_mut) {
                Some(existing) => {
                    existing.insert("include_usage".to_string(), json!(true));
                }
                None => {
                    obj.insert(
                        "stream_options".to_string(),
                        json!({ "include_usage": true }),
                    );
                }
            }
        }
        // Strip internal identifiers + client-E2EE markers so they never reach
        // Chutes inside the (encrypted) request body.
        for k in INTERNAL_KEYS {
            obj.remove(*k);
        }
        for k in ph::LEGACY_DENIED_EXTRA_KEYS {
            obj.remove(k);
        }
    } else {
        return Err("chat params did not serialize to a JSON object".to_string());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARGS: &str = r#"{"city":"Paris","note":"  keep \"exact\" "}"#;

    fn tool_call() -> Value {
        json!({
            "index": 0, "id": "call_1", "type": "function",
            "prompt_text": "LEAK",
            "function": {"name": "get_weather", "arguments": ARGS, "prompt_text": "LEAK"}
        })
    }

    fn logprobs() -> Value {
        json!({
            "unknown": "LEAK",
            "content": [{"token": "a", "logprob": -0.1, "bytes": [97], "prompt_text": "LEAK",
                "top_logprobs": [{"token": "a", "logprob": -0.1, "bytes": [97], "prompt_text": "LEAK"}]}],
            "refusal": [{"token": "n", "logprob": -1.0, "bytes": null, "prompt_text": "LEAK", "top_logprobs": []}]
        })
    }

    fn usage() -> Value {
        json!({
            "prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7,
            "prompt_text": "LEAK", "unknown_obj": {"prompt_text": "LEAK"},
            "prompt_tokens_details": {"cached_tokens": 1, "audio_tokens": 0, "prompt_text": "LEAK"},
            "completion_tokens_details": {"reasoning_tokens": 2, "audio_tokens": 0,
                "accepted_prediction_tokens": 0, "rejected_prediction_tokens": 0, "prompt_text": "LEAK"}
        })
    }

    fn message() -> Value {
        json!({
            "role": "assistant", "content": "hi",
            "tool_calls": [tool_call()],
            "function_call": {"name": "f", "arguments": ARGS, "prompt_text": "LEAK"},
            "refusal": null,
            "annotations": [{"type": "url_citation", "prompt_text": "LEAK",
                "url_citation": {"start_index": 0, "end_index": 2, "url": "u", "title": "t", "prompt_text": "LEAK"}}],
            "audio": {"data": "LEAK"}
        })
    }

    fn body(inner: &str) -> serde_json::Map<String, Value> {
        let v = json!({
            "id": "x", "object": "o", "created": 1, "model": "m",
            "choices": [{"index": 0, "finish_reason": null, "logprobs": logprobs(), inner: message()}],
            "usage": usage()
        });
        v.as_object().unwrap().clone()
    }

    #[test]
    fn nested_containers_are_allowlisted_in_message_and_delta() {
        for inner in ["message", "delta"] {
            let mut obj = body(inner);
            sanitize_response_object(&mut obj, &[], &[]);
            let ser = serde_json::to_string(&obj).unwrap();
            assert!(!ser.contains("LEAK"), "{inner}: {ser}");
            let c = &obj["choices"][0];
            let tc = &c[inner]["tool_calls"][0];
            assert_eq!(tc["index"], 0);
            assert_eq!(tc["id"], "call_1");
            assert_eq!(tc["type"], "function");
            assert_eq!(tc["function"]["name"], "get_weather");
            assert_eq!(tc["function"]["arguments"].as_str().unwrap(), ARGS);
            assert_eq!(
                c[inner]["function_call"]["arguments"].as_str().unwrap(),
                ARGS
            );
            assert_eq!(c[inner]["annotations"][0]["url_citation"]["url"], "u");
            assert!(c[inner].get("audio").is_none());
            assert_eq!(c["logprobs"]["content"][0]["top_logprobs"][0]["token"], "a");
            assert_eq!(c["logprobs"]["content"][0]["bytes"], json!([97]));
            assert_eq!(c["logprobs"]["refusal"][0]["token"], "n");
            let u = &obj["usage"];
            assert_eq!(u["total_tokens"], 7);
            assert_eq!(u["prompt_tokens_details"]["cached_tokens"], 1);
            assert_eq!(u["completion_tokens_details"]["reasoning_tokens"], 2);
        }
    }

    #[test]
    fn wrongly_shaped_containers_are_dropped_and_scalars_left_alone() {
        let mut obj = json!({"choices": [{"index": 0, "logprobs": ["LEAK"],
            "message": {"tool_calls": {"prompt_text": "LEAK"}, "function_call": ["LEAK"],
                "annotations": {"a": "LEAK"}, "refusal": {"a": "LEAK"}}}],
            "usage": {"prompt_tokens_details": ["LEAK"], "x": {"a": "LEAK"}, "y": [1], "z": 5}})
        .as_object()
        .unwrap()
        .clone();
        sanitize_response_object(&mut obj, &[], &[]);
        assert!(!serde_json::to_string(&obj).unwrap().contains("LEAK"));
        assert_eq!(obj["usage"]["z"], 5);

        // Scalars carry no nested key: untouched (keeps Chutes output stable).
        let mut obj = json!({"choices": [{"index": 0, "logprobs": 1,
            "message": {"tool_calls": 1, "function_call": 1, "annotations": 1, "refusal": "no"}}]})
        .as_object()
        .unwrap()
        .clone();
        let before = obj.clone();
        sanitize_response_object(&mut obj, &[], &[]);
        assert_eq!(obj, before);
    }
}
