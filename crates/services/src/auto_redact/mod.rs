//! Auto-redact PII in completions requests before they reach a provider,
//! then un-redact in the response so the client sees the original text.
//!
//! See `docs/auto-redact.md` (TBD) for the end-to-end design. Quick
//! summary:
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
pub use placeholders::{RedactionMap, MAX_PLACEHOLDER_LEN, PLACEHOLDER_RE};
pub use stream_unredact::StreamUnredact;

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
/// On detector failure, returns [`AutoRedactError::DetectorUnavailable`]
/// without mutating `messages`. Callers must fail closed.
pub async fn redact_messages(
    messages: &mut [CompletionMessage],
    pii_model: &str,
    pool: &InferenceProviderPool,
) -> Result<RedactionMap, AutoRedactError> {
    let (refs, texts) = apply::collect_text_fragments(messages);
    if texts.is_empty() {
        return Ok(RedactionMap::new());
    }

    let mut map = RedactionMap::new();
    // Reserve any placeholder-shaped literals already in the input so we
    // never mint a placeholder that collides with the user's own text.
    for t in &texts {
        for caps in PLACEHOLDER_RE.find_iter(t) {
            map.reserve_literal(caps.as_str());
        }
    }

    let spans_per_text = detect::detect_pii(&texts, pii_model, pool).await?;
    debug_assert_eq!(spans_per_text.len(), texts.len());

    let mut redacted = Vec::with_capacity(texts.len());
    for (text, spans) in texts.iter().zip(spans_per_text.iter()) {
        redacted.push(apply::redact_one(text, spans, &mut map));
    }
    apply::write_back(messages, &refs, redacted);
    Ok(map)
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
