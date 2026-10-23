//! Call the privacy-filter model via the inference provider pool, parsing
//! the response into per-text [`Span`] lists.

use super::apply::Span;
use super::AutoRedactError;
use crate::inference_provider_pool::InferenceProviderPool;
use serde::Deserialize;
use std::collections::HashMap;

/// Default threshold for privacy-filter spans. Below this score we discard
/// the span. Score distribution from the model concentrates near 1.0 for
/// clean PII, so 0.5 trades off mild false-negatives for strong false-
/// positive resistance.
pub const DEFAULT_THRESHOLD: f64 = 0.5;

/// Call the privacy-filter model with the list of input texts. Returns one
/// Span vec per input (in the same order) plus the total `input_tokens` the
/// model billed for the classify pass.
///
/// On any transport / parse / status error, returns `AutoRedactError`.
/// Callers should fail closed (return 503 to the client) when this errors.
pub async fn detect_pii(
    texts: &[String],
    model: &str,
    pool: &InferenceProviderPool,
) -> Result<(Vec<Vec<Span>>, i64), AutoRedactError> {
    if texts.is_empty() {
        return Ok((Vec::new(), 0));
    }

    let req_body = serde_json::json!({
        "model": model,
        "input": texts,
        "threshold": DEFAULT_THRESHOLD,
    });
    let body_bytes = serde_json::to_vec(&req_body)
        .map_err(|e| AutoRedactError::Internal(format!("encode detect req: {e}")))?;

    let resp_bytes = pool
        .privacy_classify(model, bytes::Bytes::from(body_bytes), HashMap::new())
        .await
        .map_err(|e| AutoRedactError::DetectorUnavailable(detector_error_message(&e)))?;

    parse_response(&resp_bytes, texts.len())
}

/// Convert the inference-pool error into a sanitized public message.
/// We deliberately drop any provider-specific detail that could leak
/// state about other tenants or backends.
fn detector_error_message(err: &inference_providers::PrivacyClassifyError) -> String {
    use inference_providers::PrivacyClassifyError as E;
    match err {
        E::HttpError { status_code, .. } => {
            format!("PII detector returned HTTP {status_code}")
        }
        E::RequestFailed(_) => "PII detector unreachable".to_string(),
    }
}

/// Privacy-filter response shape:
/// ```json
/// {
///   "model": "openai/privacy-filter",
///   "data": [
///     {"index": 0, "spans": [{"category": "...", "start": N, "end": N, "score": F, "text": "..."}, ...], "usage": {"input_tokens": N}},
///     ...
///   ]
/// }
/// ```
#[derive(Deserialize)]
struct DetectResponse {
    #[serde(default)]
    data: Vec<DetectItem>,
}

#[derive(Deserialize)]
struct DetectItem {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    spans: Vec<RawSpan>,
    /// Billing-only metadata, parsed as a tolerant `Value` (not a typed
    /// struct) so a malformed `usage` field — wrong type, out-of-range
    /// number — can never turn a valid span response into a parse error
    /// and fail the completion. `input_tokens` is extracted leniently in
    /// `usage_input_tokens` (invalid/absent => 0).
    #[serde(default)]
    usage: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawSpan {
    category: String,
    start: usize,
    end: usize,
}

/// One privacy-filter data entry's billable usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageEntry {
    /// The entry's `index`; `None` when present but not a valid index.
    pub index: Option<usize>,
    /// The entry's `usage.input_tokens`, non-negative.
    pub input_tokens: i64,
}

/// A data entry's billable `input_tokens`, read leniently: absent, negative
/// or non-integer values yield `None` (billing metadata must never break
/// span parsing or void other entries), and values above `i64::MAX`
/// saturate so they still reach the caller's overflow cap.
fn usage_input_tokens(usage: &serde_json::Value) -> Option<i64> {
    usage
        .get("input_tokens")?
        .as_u64()
        .map(|t| i64::try_from(t).unwrap_or(i64::MAX))
}

/// Billable usage per data entry of a privacy-filter response, with the
/// same per-entry leniency as [`parse_response`]: an entry without a usable
/// `input_tokens` is skipped without voiding the others. A missing `index`
/// defaults to 0. A body without a `data` array yields no entries.
///
/// Used by the /v1/privacy/classify and /v1/privacy/redact handlers, which
/// bill the raw upstream response themselves.
pub fn parse_usage_entries(bytes: &[u8]) -> Vec<UsageEntry> {
    #[derive(Deserialize)]
    struct UsageResponse {
        #[serde(default)]
        data: Vec<serde_json::Value>,
    }

    let Ok(parsed) = serde_json::from_slice::<UsageResponse>(bytes) else {
        return Vec::new();
    };
    parsed
        .data
        .iter()
        .filter_map(|entry| {
            let input_tokens = usage_input_tokens(entry.get("usage")?)?;
            let index = match entry.get("index") {
                None => Some(0),
                Some(v) => v.as_u64().and_then(|i| usize::try_from(i).ok()),
            };
            Some(UsageEntry {
                index,
                input_tokens,
            })
        })
        .collect()
}

/// Parse a privacy-filter response into per-text spans plus the total
/// `input_tokens` the model reported across all `data` entries. The token
/// total is summed independently of the per-index span mapping so it
/// reflects the full classify cost even if the model returns an unexpected
/// index (which is otherwise dropped). Used to bill the classify pass
/// (nearai/cloud-api#602).
pub(super) fn parse_response(
    bytes: &[u8],
    expected_len: usize,
) -> Result<(Vec<Vec<Span>>, i64), AutoRedactError> {
    let parsed: DetectResponse = serde_json::from_slice(bytes)
        .map_err(|e| AutoRedactError::Internal(format!("decode detect resp: {e}")))?;

    // Sum `input_tokens` across entries, leniently: a non-integer or absent
    // value contributes 0 (billing metadata must never break span parsing).
    // Saturating so a malicious/buggy detector returning huge or many counts
    // can't wrap to a negative total and mis-bill.
    let input_tokens: i64 = parsed
        .data
        .iter()
        .filter_map(|item| item.usage.as_ref())
        .filter_map(usage_input_tokens)
        .fold(0i64, |acc, t| acc.saturating_add(t));

    let mut out: Vec<Vec<Span>> = (0..expected_len).map(|_| Vec::new()).collect();
    for item in parsed.data {
        if item.index >= expected_len {
            // Defensive: model returned an index beyond what we asked for.
            continue;
        }
        // Extend rather than replace: the detector may legitimately split
        // detections for the same input across multiple data entries.
        out[item.index].extend(item.spans.into_iter().map(|s| Span {
            category: s.category,
            start: s.start,
            end: s.end,
        }));
    }
    Ok((out, input_tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(index: Option<usize>, input_tokens: i64) -> UsageEntry {
        UsageEntry {
            index,
            input_tokens,
        }
    }

    #[test]
    fn usage_entries_skip_only_the_malformed_entry() {
        let body = br#"{"data": [
            {"index": 0, "usage": {"input_tokens": 5}},
            {"index": 1, "usage": {"input_tokens": "n/a"}},
            {"index": 2, "usage": {"input_tokens": 3000000000}},
            {"index": 3, "usage": {"input_tokens": -4}},
            {"index": 4, "usage": {"input_tokens": 1.5}},
            {"index": 5, "usage": {}},
            {"index": 6},
            {"usage": {"input_tokens": 2}},
            {"index": "x", "usage": {"input_tokens": 3}}
        ]}"#;
        assert_eq!(
            parse_usage_entries(body),
            vec![
                entry(Some(0), 5),
                entry(Some(2), 3_000_000_000),
                entry(Some(0), 2),
                entry(None, 3),
            ]
        );
    }

    /// Counts beyond i64 saturate rather than being dropped, so they still
    /// trip the caller's overflow cap and anomaly metric.
    #[test]
    fn usage_entries_saturate_counts_above_i64_max() {
        let body = br#"{"data": [
            {"index": 0, "usage": {"input_tokens": 5}},
            {"index": 1, "usage": {"input_tokens": 18446744073709551615}}
        ]}"#;
        assert_eq!(
            parse_usage_entries(body),
            vec![entry(Some(0), 5), entry(Some(1), i64::MAX)]
        );
    }

    #[test]
    fn usage_entries_empty_for_unexpected_bodies() {
        assert!(parse_usage_entries(b"not json").is_empty());
        assert!(parse_usage_entries(br#"{"data": "nope"}"#).is_empty());
        assert!(parse_usage_entries(br#"{"model": "m"}"#).is_empty());
    }

    #[test]
    fn parse_response_saturates_counts_above_i64_max() {
        let body = br#"{"data": [
            {"index": 0, "spans": [], "usage": {"input_tokens": 18446744073709551615}},
            {"index": 1, "spans": [], "usage": {"input_tokens": 1}}
        ]}"#;
        let (_, input_tokens) = parse_response(body, 2).unwrap();
        assert_eq!(input_tokens, i64::MAX);
    }

    #[test]
    fn parse_basic_response() {
        let body = br#"{
            "model": "openai/privacy-filter",
            "data": [
                {"index": 0, "spans": [
                    {"category": "private_email", "start": 6, "end": 23, "score": 0.99, "text": "alice@example.com"}
                ], "usage": {"input_tokens": 5}},
                {"index": 1, "spans": [], "usage": {"input_tokens": 2}}
            ]
        }"#;
        let (spans, input_tokens) = parse_response(body, 2).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].len(), 1);
        assert_eq!(spans[0][0].category, "private_email");
        assert_eq!(spans[0][0].start, 6);
        assert_eq!(spans[0][0].end, 23);
        assert_eq!(spans[1].len(), 0);
        // Tokens summed across all data entries: 5 + 2.
        assert_eq!(input_tokens, 7);
    }

    #[test]
    fn parse_pads_missing_indices() {
        // If the detector omits index 1 entirely, we still return an empty
        // span vec for it so apply.write_back stays index-aligned.
        let body = br#"{"data": [{"index": 0, "spans": [], "usage": {"input_tokens": 1}}]}"#;
        let (spans, input_tokens) = parse_response(body, 3).unwrap();
        assert_eq!(spans.len(), 3);
        for s in &spans {
            assert!(s.is_empty());
        }
        assert_eq!(input_tokens, 1);
    }

    #[test]
    fn parse_drops_out_of_range_index() {
        // index=5 but we only sent 2 texts — drop it rather than panicking.
        let body = br#"{"data": [
            {"index": 5, "spans": [{"category": "x", "start": 0, "end": 1, "score": 1.0, "text": "h"}], "usage": {"input_tokens": 1}}
        ]}"#;
        let (spans, input_tokens) = parse_response(body, 2).unwrap();
        assert_eq!(spans.len(), 2);
        assert!(spans.iter().all(|v| v.is_empty()));
        // Out-of-range index is dropped for spans, but its tokens still count.
        assert_eq!(input_tokens, 1);
    }

    #[test]
    fn parse_tolerates_malformed_usage() {
        // A malformed/missing billing-only `usage` field must NOT fail span
        // parsing (which would fail the completion). Spans still apply; the
        // bad usage contributes 0 tokens. (nearai/cloud-api#602 review.)
        let body = br#"{"data": [
            {"index": 0, "spans": [{"category": "private_email", "start": 0, "end": 1, "score": 1.0, "text": "a"}], "usage": {"input_tokens": "oops"}},
            {"index": 1, "spans": [], "usage": "garbage"},
            {"index": 1, "spans": []}
        ]}"#;
        let (spans, input_tokens) = parse_response(body, 2).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].len(), 1, "span must still apply despite bad usage");
        assert_eq!(input_tokens, 0, "invalid/absent usage contributes 0");
    }

    #[test]
    fn parse_garbage_errors() {
        let err = parse_response(b"not json", 1).unwrap_err();
        assert!(matches!(err, AutoRedactError::Internal(_)));
    }
}
