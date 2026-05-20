//! Auto-redact PII in completions requests before they reach a provider,
//! then un-redact in the response so the client sees the original text.
//!
//! End-to-end flow:
//!
//! ```text
//! client → cloud-api (TEE)
//!   1. extract text fragments from messages
//!   2. call privacy-filter for PII spans
//!   3. mint placeholders <emailN>, <phoneN>, … and rewrite fragments
//!   → provider sees only redacted text
//! provider → cloud-api
//!   4. walk response.choices[*].message.content (non-stream)
//!      or wrap stream chunks with StreamUnredact (stream)
//!   5. swap placeholders back to originals
//! → client
//! ```
//!
//! The mapping is per-request, lives on the handler stack, never persisted
//! or logged.

mod apply;
mod detect;
mod placeholders;
mod stream_unredact;

pub use apply::TextRef;
pub use placeholders::{RedactionMap, MAX_PLACEHOLDER_LEN};
pub use stream_unredact::StreamUnredact;

/// Default minimum confidence score for spans the privacy-filter returns.
/// Re-exported so the `/v1/privacy/redact` handler can fill in the same
/// threshold the auto-redact path uses when the client omits it.
pub const DEFAULT_THRESHOLD_PUBLIC: f64 = detect::DEFAULT_THRESHOLD;

use crate::completions::ports::CompletionMessage;
use crate::inference_provider_pool::InferenceProviderPool;

/// Header name clients use to enable auto-redact. Equivalent to the body
/// field `auto_redact: true`.
pub const AUTO_REDACT_HEADER: &str = "x-auto-redact";

/// Body field clients can use to enable auto-redact (OpenAI-idiomatic
/// alternative to the header).
pub const AUTO_REDACT_BODY_FIELD: &str = "auto_redact";

/// Default PII model id. The model must be registered in the cloud-api
/// model catalog with an `inference_url` pointing at a privacy-filter
/// backend (see nearai/infra#86).
pub const DEFAULT_PII_MODEL: &str = "openai/privacy-filter";

/// Wall-clock budget for the entire redact step (detector call + apply).
/// The provider pool retries internally with per-attempt timeouts up to
/// `completion_timeout()` (default 600s) across multiple providers, so the
/// worst-case path without this outer bound is many minutes. Auto-redact
/// is in the critical request path, so we cap it tightly: a hung detector
/// must surface as a 503 quickly, not hold the user's request hostage.
pub const REDACT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum AutoRedactError {
    /// The PII detector is unreachable, timed out, or returned a non-2xx
    /// response. Per design, handlers must fail closed (503) when this
    /// fires.
    #[error("auto_redact unavailable: {0}")]
    DetectorUnavailable(String),
    /// Internal error (serde, programmer bug). Mapped to 500.
    #[error("auto_redact internal error: {0}")]
    Internal(String),
}

/// True if the request body or header opts into auto-redact. Treats common
/// affirmative spellings as enabled.
pub fn is_enabled<'a, I>(header_values: I, body_field: Option<&serde_json::Value>) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    if let Some(v) = body_field {
        if value_enables(v) {
            return true;
        }
    }
    for h in header_values {
        if str_enables(h) {
            return true;
        }
    }
    false
}

fn str_enables(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "on" | "true" | "1" | "yes" | "required"
    )
}

fn value_enables(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::String(s) => str_enables(s),
        serde_json::Value::Number(n) => n.as_i64() == Some(1),
        _ => false,
    }
}

/// Detect PII across all text fragments in `messages`, mutate the messages
/// to contain placeholders instead, and return the placeholder→original
/// mapping. Empty mapping means no PII was detected.
///
/// On detector failure or timeout (see [`REDACT_TIMEOUT`]), returns
/// [`AutoRedactError::DetectorUnavailable`] without mutating `messages`.
/// Callers must fail closed.
pub async fn redact_messages(
    messages: &mut [CompletionMessage],
    pii_model: &str,
    pool: &InferenceProviderPool,
) -> Result<RedactionMap, AutoRedactError> {
    match tokio::time::timeout(
        REDACT_TIMEOUT,
        redact_messages_inner(messages, pii_model, pool),
    )
    .await
    {
        Ok(result) => result,
        Err(_elapsed) => Err(AutoRedactError::DetectorUnavailable(format!(
            "PII detector exceeded {}s budget",
            REDACT_TIMEOUT.as_secs()
        ))),
    }
}

async fn redact_messages_inner(
    messages: &mut [CompletionMessage],
    pii_model: &str,
    pool: &InferenceProviderPool,
) -> Result<RedactionMap, AutoRedactError> {
    let (refs, texts) = apply::collect_text_fragments(messages);
    if texts.is_empty() {
        return Ok(RedactionMap::new());
    }

    let mut map = RedactionMap::new();
    // Concatenated haystack of every input fragment. Minting a dummy
    // refuses any candidate that already appears as a substring here,
    // preventing collisions when the user's own text happens to contain
    // a string we'd otherwise mint.
    let haystack: String = texts.join("\u{0001}");

    let spans_per_text = detect::detect_pii(&texts, pii_model, pool).await?;
    debug_assert_eq!(spans_per_text.len(), texts.len());

    let mut redacted = Vec::with_capacity(texts.len());
    for (text, spans) in texts.iter().zip(spans_per_text.iter()) {
        // redact_one returns Err on malformed spans (out-of-range or non
        // UTF-8 char boundary). Propagate so the handler fails closed
        // rather than silently passing raw PII upstream.
        redacted.push(apply::redact_one(text, spans, &mut map, &haystack)?);
    }
    apply::write_back(messages, &refs, redacted);
    Ok(map)
}

/// Apply privacy-filter spans (parsed from a raw classify response) to a
/// batch of texts. Returns one redacted string per input text, in the
/// same order. Used by the `/v1/privacy/redact` endpoint after a passthrough
/// classify call: the handler already has the response bytes for billing,
/// and this function consumes them to perform the redaction locally.
///
/// Fails closed (returns `AutoRedactError::Internal`) on malformed spans
/// or non-UTF-8 boundaries, so the caller must surface 5xx rather than
/// pass the raw text through.
pub fn apply_detected_spans(
    texts: &[String],
    response_bytes: &[u8],
) -> Result<Vec<String>, AutoRedactError> {
    let spans_per_text = detect::parse_response(response_bytes, texts.len())?;
    let mut map = RedactionMap::new();
    // Same haystack rule as redact_messages_inner: a minted dummy must
    // not appear in any input fragment, so substring substitution stays
    // unambiguous.
    let haystack: String = texts.join("\u{0001}");
    let mut out = Vec::with_capacity(texts.len());
    for (text, spans) in texts.iter().zip(spans_per_text.iter()) {
        out.push(apply::redact_one(text, spans, &mut map, &haystack)?);
    }
    Ok(out)
}

/// Strip the `auto_redact` field from a `ServiceCompletionRequest.extra`
/// map so it isn't forwarded to the upstream provider (Anthropic and
/// others 422 on unknown fields under strict mode).
pub fn strip_body_field(extra: &mut std::collections::HashMap<String, serde_json::Value>) {
    extra.remove(AUTO_REDACT_BODY_FIELD);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn is_enabled_truthy_header() {
        assert!(is_enabled(["on"], None));
        assert!(is_enabled(["true"], None));
        assert!(is_enabled(["1"], None));
        assert!(is_enabled(["YES"], None));
        assert!(is_enabled(["required"], None));
    }

    #[test]
    fn is_enabled_falsy_header() {
        assert!(!is_enabled(["off"], None));
        assert!(!is_enabled(["false"], None));
        assert!(!is_enabled([""], None));
        assert!(!is_enabled(::std::iter::empty(), None));
    }

    #[test]
    fn is_enabled_body_field() {
        assert!(is_enabled(::std::iter::empty(), Some(&json!(true))));
        assert!(is_enabled(::std::iter::empty(), Some(&json!("on"))));
        assert!(is_enabled(::std::iter::empty(), Some(&json!(1))));
        assert!(!is_enabled(::std::iter::empty(), Some(&json!(false))));
        assert!(!is_enabled(::std::iter::empty(), Some(&json!(null))));
    }

    #[test]
    fn is_enabled_header_or_body_wins() {
        // header on, body absent -> enabled
        assert!(is_enabled(["on"], None));
        // header absent, body true -> enabled
        assert!(is_enabled(::std::iter::empty(), Some(&json!(true))));
        // both off -> disabled
        assert!(!is_enabled(["off"], Some(&json!(false))));
    }

    #[test]
    fn strip_body_field_removes_only_auto_redact() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("auto_redact".to_string(), json!(true));
        extra.insert("temperature".to_string(), json!(0.5));
        strip_body_field(&mut extra);
        assert!(!extra.contains_key("auto_redact"));
        assert!(extra.contains_key("temperature"));
    }
}
