//! Walk `CompletionMessage` content, pull out the text fragments that need
//! to be sent to the PII detector, and write the redacted text back in.
//!
//! Message content in [`crate::completions::ports::CompletionMessage`] is a
//! `serde_json::Value`:
//! - `Value::String(s)` for simple text
//! - `Value::Array([{"type":"text","text":s}, …])` for multimodal parts
//!
//! Only text fragments are extracted; non-text parts (image_url, audio,
//! video, file) pass through unchanged. Order is preserved so the indices
//! returned to the detector match the indices we write back.

use crate::completions::ports::CompletionMessage;

/// A reference into a `CompletionMessage` that points at a single text
/// fragment. Pairs 1:1 with an entry in the texts vec we hand to the
/// detector.
#[derive(Debug, Clone)]
pub enum TextRef {
    /// `messages[msg_idx].content` is a `Value::String`.
    Whole { msg_idx: usize },
    /// `messages[msg_idx].content[part_idx]["text"]` is a text part.
    Part { msg_idx: usize, part_idx: usize },
}

/// Pull every text fragment from `messages` along with a reference for
/// writing it back. Non-text content (images, audio, etc.) is skipped.
pub fn collect_text_fragments(messages: &[CompletionMessage]) -> (Vec<TextRef>, Vec<String>) {
    let mut refs = Vec::new();
    let mut texts = Vec::new();

    for (msg_idx, msg) in messages.iter().enumerate() {
        match &msg.content {
            serde_json::Value::String(s) => {
                if !s.is_empty() {
                    refs.push(TextRef::Whole { msg_idx });
                    texts.push(s.clone());
                }
            }
            serde_json::Value::Array(parts) => {
                for (part_idx, part) in parts.iter().enumerate() {
                    if let Some(ty) = part.get("type").and_then(|v| v.as_str()) {
                        if ty == "text" {
                            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    refs.push(TextRef::Part { msg_idx, part_idx });
                                    texts.push(text.to_string());
                                }
                            }
                        }
                    }
                }
            }
            // Null / number / bool / object: ignore.
            _ => {}
        }
    }

    (refs, texts)
}

/// Write `redacted` back to the locations pointed at by `refs`. Lengths
/// must match (one redacted string per ref). Panics on mismatch — this is a
/// caller bug, not a runtime condition.
pub fn write_back(messages: &mut [CompletionMessage], refs: &[TextRef], redacted: Vec<String>) {
    assert_eq!(
        refs.len(),
        redacted.len(),
        "refs and redacted lengths must match"
    );

    for (r, new_text) in refs.iter().zip(redacted) {
        match r {
            TextRef::Whole { msg_idx } => {
                messages[*msg_idx].content = serde_json::Value::String(new_text);
            }
            TextRef::Part { msg_idx, part_idx } => {
                if let serde_json::Value::Array(parts) = &mut messages[*msg_idx].content {
                    if let Some(part) = parts.get_mut(*part_idx) {
                        if let Some(obj) = part.as_object_mut() {
                            obj.insert("text".to_string(), serde_json::Value::String(new_text));
                        }
                    }
                }
            }
        }
    }
}

/// A single PII span as returned by the privacy-filter model. Byte offsets
/// are into the original UTF-8 text.
#[derive(Debug, Clone)]
pub struct Span {
    pub category: String,
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// Apply detected spans to a text fragment, replacing each PII span with a
/// placeholder minted (or reused) on `map`. Spans must be sorted by
/// `start`; overlapping spans are resolved by preferring the earlier one
/// (the later one is silently dropped — privacy-filter doesn't emit
/// overlaps in practice).
pub fn redact_one(text: &str, spans: &[Span], map: &mut super::RedactionMap) -> String {
    if spans.is_empty() {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let mut sorted: Vec<&Span> = spans.iter().collect();
    sorted.sort_by_key(|s| s.start);

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for span in sorted {
        if span.start < cursor {
            // Overlapping with the previous span: drop the offending one.
            continue;
        }
        if span.end > bytes.len() || span.start > span.end {
            // Malformed span — skip rather than panic on slice.
            continue;
        }
        // Append the text between the previous span and this one.
        if let Ok(s) = std::str::from_utf8(&bytes[cursor..span.start]) {
            out.push_str(s);
        } else {
            // Non-UTF8 boundary; refuse to redact this fragment safely.
            return text.to_string();
        }
        // Prefer the model's own `text` field when it's a clean UTF-8 slice
        // of the input; fall back to the raw slice otherwise.
        let original = std::str::from_utf8(&bytes[span.start..span.end])
            .map(str::to_string)
            .unwrap_or_else(|_| span.text.clone());
        let placeholder = map.lookup_or_mint(&span.category, &original);
        out.push_str(&placeholder);
        cursor = span.end;
    }
    if cursor < bytes.len() {
        if let Ok(s) = std::str::from_utf8(&bytes[cursor..]) {
            out.push_str(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_redact::RedactionMap;
    use serde_json::json;

    fn msg(role: &str, content: serde_json::Value) -> CompletionMessage {
        CompletionMessage {
            role: role.to_string(),
            content,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    #[test]
    fn collect_string_content() {
        let messages = vec![
            msg("system", json!("be terse")),
            msg("user", json!("hello world")),
        ];
        let (refs, texts) = collect_text_fragments(&messages);
        assert_eq!(texts, vec!["be terse", "hello world"]);
        assert!(matches!(refs[0], TextRef::Whole { msg_idx: 0 }));
        assert!(matches!(refs[1], TextRef::Whole { msg_idx: 1 }));
    }

    #[test]
    fn collect_skips_non_text_parts() {
        let messages = vec![msg(
            "user",
            json!([
                {"type": "text", "text": "look at this"},
                {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}},
                {"type": "text", "text": "and this"}
            ]),
        )];
        let (refs, texts) = collect_text_fragments(&messages);
        assert_eq!(texts, vec!["look at this", "and this"]);
        assert!(matches!(
            refs[0],
            TextRef::Part {
                msg_idx: 0,
                part_idx: 0
            }
        ));
        assert!(matches!(
            refs[1],
            TextRef::Part {
                msg_idx: 0,
                part_idx: 2
            }
        ));
    }

    #[test]
    fn write_back_round_trip() {
        let mut messages = vec![
            msg("user", json!("alpha")),
            msg(
                "user",
                json!([
                    {"type": "text", "text": "beta"},
                    {"type": "image_url", "image_url": {"url": "x"}},
                    {"type": "text", "text": "gamma"}
                ]),
            ),
        ];
        let (refs, texts) = collect_text_fragments(&messages);
        assert_eq!(texts.len(), 3);
        let new_texts = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        write_back(&mut messages, &refs, new_texts);

        assert_eq!(messages[0].content, json!("A"));
        let arr = messages[1].content.as_array().unwrap();
        assert_eq!(arr[0]["text"], json!("B"));
        assert_eq!(arr[1]["type"], json!("image_url"), "image part untouched");
        assert_eq!(arr[2]["text"], json!("C"));
    }

    #[test]
    fn redact_one_basic() {
        let mut map = RedactionMap::new();
        let text = "Email alice@example.com or bob@example.com";
        let spans = vec![
            Span {
                category: "private_email".into(),
                start: 6,
                end: 23,
                text: "alice@example.com".into(),
            },
            Span {
                category: "private_email".into(),
                start: 27,
                end: 42,
                text: "bob@example.com".into(),
            },
        ];
        let out = redact_one(text, &spans, &mut map);
        assert_eq!(out, "Email <email1> or <email2>");
    }

    #[test]
    fn redact_one_dedup_same_email() {
        let mut map = RedactionMap::new();
        let text = "to alice@x.com or alice@x.com again";
        let spans = vec![
            Span {
                category: "private_email".into(),
                start: 3,
                end: 14,
                text: "alice@x.com".into(),
            },
            Span {
                category: "private_email".into(),
                start: 18,
                end: 29,
                text: "alice@x.com".into(),
            },
        ];
        let out = redact_one(text, &spans, &mut map);
        assert_eq!(out, "to <email1> or <email1> again");
    }

    #[test]
    fn redact_one_drops_overlapping_span() {
        let mut map = RedactionMap::new();
        let text = "Hello world";
        // Two spans where the second overlaps the first.
        let spans = vec![
            Span {
                category: "private_name".into(),
                start: 0,
                end: 5,
                text: "Hello".into(),
            },
            Span {
                category: "private_name".into(),
                start: 2,
                end: 7,
                text: "llo w".into(),
            },
        ];
        let out = redact_one(text, &spans, &mut map);
        assert_eq!(out, "<name1> world");
    }

    #[test]
    fn redact_one_empty_spans_passthrough() {
        let mut map = RedactionMap::new();
        let out = redact_one("nothing private", &[], &mut map);
        assert_eq!(out, "nothing private");
        assert!(map.is_empty());
    }
}
