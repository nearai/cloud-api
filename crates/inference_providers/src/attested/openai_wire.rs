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

/// Sanitize a response/chunk object to the standard OpenAI shape by ALLOWLIST, in
/// place: the top level keeps [`TOP_LEVEL_FIELDS`] plus `extra_top`, each
/// `choices[]` element keeps [`CHOICE_FIELDS`], and each `choices[].message` /
/// `choices[].delta` keeps [`MESSAGE_FIELDS`] plus `extra_message`. Everything
/// else, known internals and unknown future ones alike, is dropped.
///
/// Conservative under-strip: when in doubt a field is added to the allowlist
/// (keeping a stray field is safer than dropping a legitimate one), but the known
/// serving internals are deliberately absent and therefore dropped.
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
                // Recurse into the message (non-stream) and delta (stream) shapes.
                for inner in ["message", "delta"] {
                    if let Some(m) = choice_obj.get_mut(inner).and_then(Value::as_object_mut) {
                        m.retain(|k, _| {
                            MESSAGE_FIELDS.contains(&k.as_str())
                                || extra_message.contains(&k.as_str())
                        });
                    }
                }
            }
        }
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
