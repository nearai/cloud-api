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

/// Call the privacy-filter model with the list of input texts. Returns
/// one Span vec per input, in the same order.
///
/// On any transport / parse / status error, returns `AutoRedactError`.
/// Callers should fail closed (return 503 to the client) when this errors.
pub async fn detect_pii(
    texts: &[String],
    model: &str,
    pool: &InferenceProviderPool,
) -> Result<Vec<Vec<Span>>, AutoRedactError> {
    if texts.is_empty() {
        return Ok(Vec::new());
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
}

#[derive(Deserialize)]
struct RawSpan {
    category: String,
    start: usize,
    end: usize,
    #[serde(default)]
    text: String,
}

fn parse_response(bytes: &[u8], expected_len: usize) -> Result<Vec<Vec<Span>>, AutoRedactError> {
    let parsed: DetectResponse = serde_json::from_slice(bytes)
        .map_err(|e| AutoRedactError::Internal(format!("decode detect resp: {e}")))?;

    let mut out: Vec<Vec<Span>> = (0..expected_len).map(|_| Vec::new()).collect();
    for item in parsed.data {
        if item.index >= expected_len {
            // Defensive: model returned an index beyond what we asked for.
            continue;
        }
        out[item.index] = item
            .spans
            .into_iter()
            .map(|s| Span {
                category: s.category,
                start: s.start,
                end: s.end,
                text: s.text,
            })
            .collect();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let spans = parse_response(body, 2).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].len(), 1);
        assert_eq!(spans[0][0].category, "private_email");
        assert_eq!(spans[0][0].start, 6);
        assert_eq!(spans[0][0].end, 23);
        assert_eq!(spans[1].len(), 0);
    }

    #[test]
    fn parse_pads_missing_indices() {
        // If the detector omits index 1 entirely, we still return an empty
        // span vec for it so apply.write_back stays index-aligned.
        let body = br#"{"data": [{"index": 0, "spans": [], "usage": {"input_tokens": 1}}]}"#;
        let spans = parse_response(body, 3).unwrap();
        assert_eq!(spans.len(), 3);
        for s in &spans {
            assert!(s.is_empty());
        }
    }

    #[test]
    fn parse_drops_out_of_range_index() {
        // index=5 but we only sent 2 texts — drop it rather than panicking.
        let body = br#"{"data": [
            {"index": 5, "spans": [{"category": "x", "start": 0, "end": 1, "score": 1.0, "text": "h"}], "usage": {"input_tokens": 1}}
        ]}"#;
        let spans = parse_response(body, 2).unwrap();
        assert_eq!(spans.len(), 2);
        assert!(spans.iter().all(|v| v.is_empty()));
    }

    #[test]
    fn parse_garbage_errors() {
        let err = parse_response(b"not json", 1).unwrap_err();
        assert!(matches!(err, AutoRedactError::Internal(_)));
    }
}
