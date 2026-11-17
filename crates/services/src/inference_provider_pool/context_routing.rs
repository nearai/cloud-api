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
//!
//! Organizations at negative scheduler priority (`ChatRoutingHints.request_priority
//! < 0`) never fall back to the OTHER NEAR tier on a RETRYABLE error (5xx,
//! timeout, queue-full): [`pin_near_tier_for_low_priority`] computes the single
//! NEAR capacity the request's estimated size selects, and the retry loop in
//! `mod.rs` (`get_providers_with_fallback` / the provider-attempt loop) skips
//! NEAR candidates outside that capacity (the attested third-party fallback,
//! e.g. Chutes, is unaffected). This keeps a burst of oversized low-priority
//! requests from spilling a saturated long-context host's retryable errors
//! onto the interactive base fleet.
//!
//! The pin does NOT block the existing context-length-400 self-heal: a
//! request that context-400s on its pinned tier but has a strictly larger
//! declared NEAR sibling still falls through to it, exactly as for
//! priority >= 0 requests (see `mod.rs`'s `larger_ctx_sibling_exists` /
//! `ctx_400_falls_through`). Sizing is prompt-only (`max_tokens` is ignored),
//! so such a 400 means either the prompt estimate was too low (byte heuristic
//! error) or prompt plus the requested output exceeds the base tier. Neither
//! is a genuinely oversized request when a larger tier exists, so it
//! deserves the same self-heal as any other priority, never a hard client
//! error for a request the other tier would have served.
//!
//! This module is the only owner of the prompt-size estimate and the tier
//! boundary ([`base_capacity`]). Placement reads
//! `ChatCompletionParams.placement` (built by [`placement_context`]); it never
//! re-estimates. The service's `estimate_input_tokens` seeds
//! `hints.estimated_tokens` for single-capacity sorting only.

use std::sync::OnceLock;

use inference_providers::{ChatCompletionParams, PlacementContext};

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
    /// tool definitions).
    pub countable_tokens: u64,
    /// Flat `media_part_tokens()` per non-text content part plus ~4
    /// tokens/message chat-template overhead.
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
        // Prior-turn reasoning is part of the prompt the engine renders.
        if let Some(reasoning) = &m.reasoning_content {
            bytes += reasoning.len();
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

/// Estimate input tokens for metric labels only; routing keeps its own estimate.
pub(crate) fn metric_input_tokens(params: &ChatCompletionParams) -> u32 {
    let estimate = estimate_input(params);
    let forwarded_tool_tokens = if params.tools.is_none() {
        params
            .extra
            .get("tools")
            .and_then(|tools| serde_json::to_string(tools).ok())
            .map_or(0, |tools| {
                u64::try_from(tools.len() / 4).unwrap_or(u64::MAX)
            })
    } else {
        0
    };
    let total = estimate
        .countable_tokens
        .saturating_add(estimate.uncounted_tokens)
        .saturating_add(forwarded_tool_tokens);
    u32::try_from(total).unwrap_or(u32::MAX)
}

/// The base tier's capacity: the smallest DECLARED context capacity among a
/// model's providers, or `None` when fewer than two distinct capacities are
/// declared (a single-tier model has no tier boundary). The one tier
/// boundary: a request is heavy when its context requirement exceeds it, and
/// a provider is the long tier when its declared capacity exceeds it.
pub(crate) fn base_capacity(caps: impl IntoIterator<Item = Option<u32>>) -> Option<u32> {
    let distinct: std::collections::BTreeSet<u32> = caps.into_iter().flatten().collect();
    if distinct.len() < 2 {
        return None;
    }
    distinct.first().copied()
}

/// Whether a prompt estimate exceeds the base tier ([`base_capacity`]):
/// the request's class for placement and the tier-refinement log. Never true
/// for a single-tier model (the metric tag uses [`exceeds_declared_capacity`]).
pub(crate) fn is_heavy(prompt_tokens: u64, caps: impl IntoIterator<Item = Option<u32>>) -> bool {
    base_capacity(caps).is_some_and(|base| prompt_tokens > u64::from(base))
}

/// Whether a prompt estimate exceeds at least one declared capacity. This
/// is the `context_tier:long` metric tag's predicate: unlike [`is_heavy`] it
/// also holds for an oversized request on a single-capacity model.
pub(crate) fn exceeds_declared_capacity(
    prompt_tokens: u64,
    caps: impl IntoIterator<Item = Option<u32>>,
) -> bool {
    caps.into_iter()
        .flatten()
        .any(|cap| prompt_tokens > u64::from(cap))
}

/// The one size formula, shared by the pool's tier sort and placement:
/// `prompt_tokens = ceil(countable × safety_factor) + uncounted`. Input only:
/// the requested output length (`max_tokens`) never enters routing, and no
/// tokenizer is consulted.
pub(crate) fn requirement(estimate: &InputEstimate) -> u64 {
    ((estimate.countable_tokens as f64 * safety_factor()).ceil() as u64)
        .saturating_add(estimate.uncounted_tokens)
}

/// The typed placement context for a chat request: its size
/// ([`requirement`]) and its class: `prefill_heavy` iff the prompt exceeds
/// [`base_capacity`]; never for a single-tier model. `caps` are the model's
/// providers' declared capacities. Keeps the affinity already on `params`.
pub(crate) fn placement_context(
    caps: &[Option<u32>],
    params: &ChatCompletionParams,
) -> PlacementContext {
    let prompt_tokens = requirement(&estimate_input(params));
    PlacementContext {
        prompt_tokens: Some(prompt_tokens),
        prefill_heavy: is_heavy(prompt_tokens, caps.iter().copied()),
        ..params.placement.clone()
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

/// Decide the single NEAR capacity a negative-priority request's estimated
/// size selects (module docs above). `candidates` is `(is_near_tier,
/// declared_max_context_tokens)` per provider, in the pool's existing
/// candidate order.
///
/// Mirrors `refine_context_requirement`'s `distinct` set: only DECLARED
/// capacities create tiers. A NEAR candidate with no declared
/// `max_context_tokens` (`None`) never counts toward the tier decision and
/// is not represented in the returned capacity — callers must keep such a
/// candidate regardless of the result (it may be the model's only usable
/// NEAR provider, or a not-yet-warmed one). A model whose DECLARED NEAR
/// capacities are all equal (or number fewer than two) has nothing to pin
/// between: returns `None`, and the caller must not filter anything.
///
/// Otherwise returns `Some` of the smallest declared capacity that is `>=
/// estimated_tokens` (the tier the request's size selects), or the largest
/// declared capacity if the estimate doesn't fit any of them — mirroring the
/// "nothing fits: closest first" rule the capacity sort already applies.
///
/// Pure and pool-free so it can be unit-tested directly. The caller (the
/// retry loop in `mod.rs`) skips NEAR candidates whose declared capacity
/// differs from the returned one, with an exception for the existing
/// context-length-400 fall-through (module docs above).
pub(crate) fn pin_near_tier_for_low_priority(
    candidates: &[(bool, Option<u32>)],
    estimated_tokens: u32,
) -> Option<u32> {
    let mut near_capacities: Vec<u32> = candidates
        .iter()
        .filter(|(is_near, _)| *is_near)
        .filter_map(|(_, cap)| *cap)
        .collect();
    near_capacities.sort_unstable();
    near_capacities.dedup();

    if near_capacities.len() < 2 {
        return None;
    }

    Some(
        near_capacities
            .iter()
            .copied()
            .find(|&cap| cap >= estimated_tokens)
            .unwrap_or_else(|| *near_capacities.last().expect("checked len >= 2 above")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn metric_input_tokens_counts_tool_schemas_tool_call_args_and_media() {
        let tools = (0..15)
            .map(|index| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": format!("tool_{index}"),
                        "description": "x".repeat(600),
                        "parameters": {"type": "object", "properties": {}}
                    }
                })
            })
            .collect::<Vec<_>>();
        let params: ChatCompletionParams = serde_json::from_value(serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "Hi"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "tool_0",
                            "arguments": format!("{{\"value\":\"{}\"}}", "x".repeat(200))
                        }
                    }]
                },
                {
                    "role": "user",
                    "content": [{"type": "image_url", "image_url": {"url": "https://example.com/image.png"}}]
                }
            ],
            "tools": tools
        }))
        .unwrap();

        let metric_tokens = metric_input_tokens(&params);
        assert!((3_000..=4_000).contains(&metric_tokens));
        let estimate = estimate_input(&params);
        assert_eq!(
            metric_tokens,
            u32::try_from(estimate.countable_tokens + estimate.uncounted_tokens).unwrap()
        );

        let mut without_tools = params.clone();
        without_tools.tools = None;
        assert!(metric_tokens > metric_input_tokens(&without_tools));

        let mut without_tool_calls = params.clone();
        without_tool_calls.messages[1].tool_calls = None;
        assert!(metric_tokens > metric_input_tokens(&without_tool_calls));

        let mut without_media = params;
        without_media.messages[2].content = Some(serde_json::json!(""));
        assert!(metric_tokens > metric_input_tokens(&without_media));
    }

    #[test]
    fn metric_input_tokens_counts_forwarded_tools_only_when_typed_tools_are_absent() {
        let mut params: ChatCompletionParams = serde_json::from_value(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}]
        }))
        .unwrap();
        let forwarded_tools = serde_json::json!([{
            "type": "web_context_search",
            "description": "x".repeat(8_000)
        }]);
        let forwarded_size = serde_json::to_string(&forwarded_tools).unwrap().len() / 4;
        params.extra.insert("tools".to_string(), forwarded_tools);

        assert_eq!(
            metric_input_tokens(&params),
            u32::try_from(forwarded_size + 4).unwrap()
        );

        params.tools = Some(
            serde_json::from_value(serde_json::json!([{
                "type": "function",
                "function": {
                    "name": "typed",
                    "description": "small",
                    "parameters": {"type": "object", "properties": {}}
                }
            }]))
            .unwrap(),
        );
        assert!(metric_input_tokens(&params) < 100);
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
    fn prior_reasoning_counts_toward_input_estimates() {
        let base: ChatCompletionParams = serde_json::from_value(serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "content": null, "tool_calls": []},
                {"role": "tool", "tool_call_id": "c", "content": "r"}
            ]
        }))
        .unwrap();
        let mut with_reasoning = base.clone();
        with_reasoning.messages[1].reasoning_content = Some("x".repeat(4_000));

        let before = estimate_input(&base);
        let after = estimate_input(&with_reasoning);
        assert_eq!(after.countable_tokens, before.countable_tokens + 1_000);
        assert_eq!(after.uncounted_tokens, before.uncounted_tokens);
    }

    #[test]
    fn pin_near_tier_keeps_smallest_fitting_near_capacity() {
        let candidates = [
            (true, Some(262_144)),
            (true, Some(1_048_576)),
            (false, Some(1_048_576)),
        ];
        assert_eq!(
            pin_near_tier_for_low_priority(&candidates, 10_000),
            Some(262_144)
        );
    }

    #[test]
    fn pin_near_tier_keeps_largest_capacity_when_nothing_fits() {
        let candidates = [
            (true, Some(262_144)),
            (true, Some(1_048_576)),
            (false, Some(1_048_576)),
        ];
        assert_eq!(
            pin_near_tier_for_low_priority(&candidates, 2_000_000),
            Some(1_048_576)
        );
    }

    #[test]
    fn pin_near_tier_is_noop_with_one_distinct_near_capacity() {
        // Two NEAR candidates at the same capacity: nothing to pin between.
        let candidates = [(true, Some(262_144)), (true, Some(262_144)), (false, None)];
        assert_eq!(pin_near_tier_for_low_priority(&candidates, 10_000), None);
    }

    #[test]
    fn pin_near_tier_is_noop_with_a_single_near_candidate() {
        let candidates = [(true, Some(262_144)), (false, Some(1_048_576))];
        assert_eq!(pin_near_tier_for_low_priority(&candidates, 10_000), None);
    }

    #[test]
    fn pin_near_tier_ignores_undeclared_capacity_phantom_tier() {
        // A NEAR candidate with NO declared capacity must never be treated as
        // a distinct (u32::MAX) tier to pin away from — only declared
        // capacities create tiers (mirrors `refine_context_requirement`'s
        // `distinct` set). With only one DECLARED NEAR capacity here, there
        // is nothing to pin between, regardless of the estimate, and the
        // caller must keep the undeclared-capacity candidate.
        let candidates = [(true, Some(262_144)), (true, None)];
        assert_eq!(pin_near_tier_for_low_priority(&candidates, 2_000_000), None);
    }
}
