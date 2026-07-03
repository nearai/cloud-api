//! Context-length tier routing: providerConfig `long_context` expansion and
//! the request-size refinement knobs.
//!
//! A NEAR-served model can run two capacity tiers behind one canonical id
//! (e.g. `z-ai/glm-5.2`: a 262k-context 2xTP4 fleet plus a 1M-context TP8
//! host on its own `*-long` domain). The tiers are declared on the model
//! row's `provider_config`:
//!
//! ```json
//! {
//!   "long_context": {
//!     "inference_url": "https://glm-5-2-long.completions.near.ai",
//!     "max_context_tokens": 1048576,
//!     "base_max_context_tokens": 262144
//!   }
//! }
//! ```
//!
//! [`expand_inference_endpoints`] turns one catalog row into the
//! `(model_name, inference_url, max_context)` entries the pool registers —
//! the base entry keeps the row's `inference_url` with
//! `base_max_context_tokens` as its declared capacity (the catalog
//! `context_length` stays the customer-facing maximum — the long tier's
//! window), and the long entry adds a second provider under the same id. The
//! pool's routing sort (see `get_providers_with_fallback`) then keeps short
//! requests on the base fleet and sends requests that don't fit its window
//! to the long tier, with the pinned attested fallback (Chutes) behind both.
//!
//! Without a `long_context` block this is the identity expansion — every
//! other model registers exactly as before.

use std::sync::OnceLock;

use inference_providers::ChatCompletionParams;

/// providerConfig key holding the long-context tier declaration. Snake_case
/// like the other `provider_config` contents (`base_url`, `model_name`).
const LONG_CONTEXT_KEY: &str = "long_context";

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(default)
}

/// Multiplier applied to the byte-based heuristic estimate before comparing
/// against provider capacities (absorbs tokenizer variance; bytes/4 can
/// underestimate code-heavy or CJK-heavy prompts by ~25%).
pub(crate) fn safety_factor() -> f64 {
    static V: OnceLock<f64> = OnceLock::new();
    *V.get_or_init(|| env_f64("CONTEXT_ROUTE_SAFETY_FACTOR", 1.2))
}

/// Multiplier applied to an exact `/v1/tokenize` count (chat-template
/// serialization overhead only, so much tighter than the heuristic factor).
pub(crate) fn exact_factor() -> f64 {
    static V: OnceLock<f64> = OnceLock::new();
    *V.get_or_init(|| env_f64("CONTEXT_ROUTE_EXACT_FACTOR", 1.05))
}

/// Band around a declared capacity (as `low*cap ..= high*cap`) inside which
/// the heuristic is too coarse to make the tier decision and the pool asks
/// the backend for an exact token count. Outside the band the heuristic's
/// error cannot flip the decision, so the extra round-trip is skipped.
pub(crate) fn tokenize_band() -> (f64, f64) {
    static V: OnceLock<(f64, f64)> = OnceLock::new();
    *V.get_or_init(|| {
        let low = env_f64("CONTEXT_ROUTE_TOKENIZE_BAND_LOW", 0.7);
        let high = env_f64("CONTEXT_ROUTE_TOKENIZE_BAND_HIGH", 1.3);
        if low < high {
            (low, high)
        } else {
            (0.7, 1.3)
        }
    })
}

/// Flat token cost assumed per non-text content part (image/audio/data URI)
/// in the byte-based estimate. Byte-counting base64 media would wildly
/// overestimate (a single image would look like ~250k tokens).
pub(crate) fn media_part_tokens() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("CONTEXT_ROUTE_MEDIA_PART_TOKENS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1024)
    })
}

/// Decomposed byte-based input estimate for the tier decision. Computed ONLY
/// inside the pool's multi-capacity refinement — the service-side
/// `ChatRoutingHints.estimated_tokens` keeps its original (text-only)
/// semantics so single-capacity models route exactly as before.
pub(crate) struct InputEstimate {
    /// bytes/4 over the countable text (message contents, tool-call args,
    /// tool definitions) — the part an exact `/v1/tokenize` count replaces.
    pub countable_tokens: u64,
    /// Flat `media_part_tokens()` per non-text content part plus ~4
    /// tokens/message chat-template overhead — added on BOTH the heuristic
    /// and the exact path (the tokenizer never sees media or the template).
    pub uncounted_tokens: u64,
}

/// Byte-based input estimate over everything that occupies the context
/// window: UTF-8 **bytes**/4 for text (bytes rather than chars keeps
/// CJK-heavy prompts, ~3 bytes/char at ~1 token/char, within range),
/// serialized tool-calls/definitions as text, flat media cost, and
/// per-message template overhead.
pub(crate) fn estimate_input(params: &ChatCompletionParams) -> InputEstimate {
    let mut bytes: usize = 0;
    let mut media_parts: u64 = 0;
    for m in &params.messages {
        match &m.content {
            Some(serde_json::Value::String(s)) => bytes += s.len(),
            Some(serde_json::Value::Array(parts)) => {
                for p in parts {
                    match p.get("text").and_then(|t| t.as_str()) {
                        Some(s) => bytes += s.len(),
                        None => media_parts += 1,
                    }
                }
            }
            _ => {}
        }
        if let Some(tool_calls) = &m.tool_calls {
            bytes += serde_json::to_string(tool_calls).map_or(0, |s| s.len());
        }
    }
    if let Some(tools) = &params.tools {
        bytes += serde_json::to_string(tools).map_or(0, |s| s.len());
    }
    InputEstimate {
        countable_tokens: (bytes / 4) as u64,
        uncounted_tokens: media_parts * media_part_tokens() + params.messages.len() as u64 * 4,
    }
}

/// Expand one catalog row into the `(model_name, inference_url, max_context)`
/// endpoint entries to register. Identity expansion unless `provider_config`
/// carries a valid `long_context` block (see module docs).
///
/// The long entry requires ALL of: a non-empty `inference_url` different from
/// the base URL, a `base_max_context_tokens`, and a strictly larger long
/// capacity (`max_context_tokens`, defaulting to the row's `context_length` —
/// the catalog value, which for a long-context model is the full window the
/// long tier serves). An invalid block is dropped WITHOUT touching the base
/// entry: without a smaller declared base capacity the two tiers would sort
/// as equals and round-robin ~half of ALL traffic onto the (typically
/// single-host) long tier, which is exactly what this routing exists to
/// prevent. Fail toward yesterday's behavior, loudly.
pub fn expand_inference_endpoints(
    model_name: &str,
    inference_url: &str,
    context_length: Option<u32>,
    provider_config: Option<&serde_json::Value>,
) -> Vec<(String, String, Option<u32>)> {
    let long = provider_config.and_then(|cfg| cfg.get(LONG_CONTEXT_KEY));

    let get_u32 = |obj: &serde_json::Value, key: &str| -> Option<u32> {
        obj.get(key)
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok())
            .filter(|v| *v > 0)
    };

    let mut out = vec![(
        model_name.to_string(),
        inference_url.to_string(),
        context_length,
    )];

    let Some(long) = long else {
        return out;
    };

    let long_url = long
        .get("inference_url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|u| !u.is_empty() && *u != inference_url);
    let base_ctx = get_u32(long, "base_max_context_tokens");
    let long_ctx = get_u32(long, "max_context_tokens").or(context_length);

    match (long_url, base_ctx, long_ctx) {
        (Some(long_url), Some(base_ctx), Some(long_ctx)) if base_ctx < long_ctx => {
            out[0].2 = Some(base_ctx);
            out.push((model_name.to_string(), long_url.to_string(), Some(long_ctx)));
        }
        _ => {
            // Numbers/model only — never customer data.
            tracing::warn!(
                model = %model_name,
                has_url = long_url.is_some(),
                base_max_context_tokens = ?base_ctx,
                max_context_tokens = ?long_ctx,
                "Ignoring invalid provider_config.long_context block \
                 (needs a distinct inference_url and base_max_context_tokens < max_context_tokens)"
            );
        }
    }

    out
}

/// Concatenate the request's countable text — message contents (string or
/// `text` content parts), tool-call arguments, and tool definitions — for an
/// exact `/v1/tokenize` count. This is CUSTOMER CONTENT: it must only be
/// sent over a provider's attested transport and must never be logged.
pub(crate) fn concat_prompt_text(params: &ChatCompletionParams) -> String {
    let mut text = String::new();
    for msg in &params.messages {
        match &msg.content {
            Some(serde_json::Value::String(s)) => text.push_str(s),
            Some(serde_json::Value::Array(parts)) => {
                for part in parts {
                    if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                    }
                }
            }
            _ => {}
        }
        if let Some(tool_calls) = &msg.tool_calls {
            if let Ok(s) = serde_json::to_string(tool_calls) {
                text.push_str(&s);
            }
        }
        text.push('\n');
    }
    if let Some(tools) = &params.tools {
        if let Ok(s) = serde_json::to_string(tools) {
            text.push_str(&s);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn expand_without_provider_config_is_identity() {
        let out = expand_inference_endpoints("m", "https://m.example", Some(131072), None);
        assert_eq!(
            out,
            vec![(
                "m".to_string(),
                "https://m.example".to_string(),
                Some(131072)
            )]
        );
    }

    #[test]
    fn expand_without_long_context_key_is_identity() {
        let pc = cfg(r#"{"something_else": true}"#);
        let out = expand_inference_endpoints("m", "https://m.example", Some(131072), Some(&pc));
        assert_eq!(
            out,
            vec![(
                "m".to_string(),
                "https://m.example".to_string(),
                Some(131072)
            )]
        );
    }

    #[test]
    fn expand_long_context_adds_second_endpoint_and_overrides_base_capacity() {
        let pc = cfg(r#"{"long_context": {
                "inference_url": "https://m-long.example",
                "max_context_tokens": 1048576,
                "base_max_context_tokens": 262144
            }}"#);
        // Catalog context_length stays the customer-facing 1M; the base
        // fleet's declared capacity comes from base_max_context_tokens.
        let out = expand_inference_endpoints("m", "https://m.example", Some(1048576), Some(&pc));
        assert_eq!(
            out,
            vec![
                (
                    "m".to_string(),
                    "https://m.example".to_string(),
                    Some(262144)
                ),
                (
                    "m".to_string(),
                    "https://m-long.example".to_string(),
                    Some(1048576)
                ),
            ]
        );
    }

    #[test]
    fn expand_long_capacity_defaults_to_catalog_context_length() {
        // max_context_tokens omitted → catalog context_length (the long
        // tier's window) fills in; base_max_context_tokens is still required.
        let pc = cfg(r#"{"long_context": {
                "inference_url": "https://m-long.example",
                "base_max_context_tokens": 262144
            }}"#);
        let out = expand_inference_endpoints("m", "https://m.example", Some(1048576), Some(&pc));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].2, Some(262144));
        assert_eq!(out[1].2, Some(1048576));
    }

    #[test]
    fn expand_drops_invalid_long_blocks_without_touching_the_base_entry() {
        for pc in [
            // No URL / empty URL / same URL as base.
            cfg(
                r#"{"long_context": {"max_context_tokens": 1048576, "base_max_context_tokens": 262144}}"#,
            ),
            cfg(r#"{"long_context": {"inference_url": "", "base_max_context_tokens": 262144}}"#),
            cfg(
                r#"{"long_context": {"inference_url": "https://m.example", "base_max_context_tokens": 262144}}"#,
            ),
            // Missing base capacity: both tiers would sort as equals and
            // round-robin short traffic onto the single long host.
            cfg(r#"{"long_context": {"inference_url": "https://m-long.example"}}"#),
            // base >= long: same failure mode.
            cfg(
                r#"{"long_context": {"inference_url": "https://m-long.example", "base_max_context_tokens": 1048576}}"#,
            ),
            // Invalid capacity values.
            cfg(
                r#"{"long_context": {"inference_url": "https://m-long.example", "max_context_tokens": 0, "base_max_context_tokens": -5}}"#,
            ),
        ] {
            let out =
                expand_inference_endpoints("m", "https://m.example", Some(1048576), Some(&pc));
            assert_eq!(
                out,
                vec![(
                    "m".to_string(),
                    "https://m.example".to_string(),
                    Some(1048576)
                )],
                "invalid long block must leave the identity expansion for {pc}"
            );
        }
    }

    #[test]
    fn concat_prompt_text_covers_content_forms_tool_calls_and_tools() {
        let params: ChatCompletionParams = serde_json::from_value(serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": [
                    {"type": "text", "text": "part1"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
                ]},
            ],
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {}}}]
        }))
        .unwrap();
        let text = concat_prompt_text(&params);
        assert!(text.contains("sys"));
        assert!(text.contains("part1"));
        assert!(
            !text.contains("base64"),
            "media payloads must not be counted as text"
        );
        assert!(
            text.contains("\"f\""),
            "tool definitions count toward context"
        );
    }
}
