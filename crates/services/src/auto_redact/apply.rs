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
    /// `messages[msg_idx].tool_calls[tc_idx].arguments` — a JSON-encoded
    /// string from a prior assistant turn. In agent loops the user resubmits
    /// the model's previous tool_call as part of conversation history, and
    /// it can echo PII from the original prompt. We redact the whole
    /// arguments string as opaque text; minted dummies are category-shaped
    /// realistic values (e.g. `redacted1@example.com`) that fit inside
    /// the JSON string without needing JSON-escaping.
    ToolCallArg { msg_idx: usize, tc_idx: usize },
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

        // Also walk this message's tool_calls. An assistant message
        // resubmitted as conversation history can carry PII verbatim in
        // its `arguments` string (e.g. `{"to":"alice@example.com"}` from a
        // prior turn). Without this, agent loops leak the original email
        // upstream on every follow-up.
        if let Some(tcs) = &msg.tool_calls {
            for (tc_idx, tc) in tcs.iter().enumerate() {
                if !tc.arguments.is_empty() {
                    refs.push(TextRef::ToolCallArg { msg_idx, tc_idx });
                    texts.push(tc.arguments.clone());
                }
            }
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
            TextRef::ToolCallArg { msg_idx, tc_idx } => {
                if let Some(tcs) = &mut messages[*msg_idx].tool_calls {
                    if let Some(tc) = tcs.get_mut(*tc_idx) {
                        tc.arguments = new_text;
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
}

/// Fold contiguous same-category spans into one. The privacy-filter
/// frequently returns one PII as multiple adjacent spans (e.g.
/// `alice@example.com` → `(0..16, private_email, "alice@example")` +
/// `(16..20, private_email, ".com")`). Without merging, the model
/// receives two placeholders for a single logical entity, which can
/// cause it to issue two parallel tool calls (one per placeholder) or
/// to strip the dummy text. Merging produces one dummy per entity.
///
/// Spans are merged when:
/// - Same `category`
/// - `prev.end == curr.start` (strictly contiguous, no gap)
fn merge_adjacent_same_category(mut spans: Vec<Span>) -> Vec<Span> {
    if spans.len() < 2 {
        return spans;
    }
    spans.sort_by_key(|s| s.start);
    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(prev) if prev.category == span.category && prev.end == span.start => {
                prev.end = span.end;
            }
            _ => merged.push(span),
        }
    }
    merged
}

/// Apply detected spans to a text fragment, replacing each PII span with
/// a placeholder minted (or reused) on `map`. Adjacent same-category
/// spans are merged into one before minting.
///
/// `haystack` is the concatenation of every input text in the request;
/// minting refuses any dummy that already appears as a substring of it.
///
/// **Fail-closed on malformed input.** If the detector returns spans
/// whose byte offsets don't land on UTF-8 char boundaries, or whose
/// bounds are outside the input, this returns
/// `Err(AutoRedactError::Internal)` rather than silently passing the
/// raw text through (which would leak PII to the upstream provider).
pub fn redact_one(
    text: &str,
    spans: &[Span],
    map: &mut super::RedactionMap,
    haystack: &str,
) -> Result<String, super::AutoRedactError> {
    use super::AutoRedactError;
    if spans.is_empty() {
        return Ok(text.to_string());
    }
    let merged = merge_adjacent_same_category(spans.to_vec());
    let bytes = text.as_bytes();

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for span in merged {
        if span.start < cursor {
            // Overlapping with the previous span: drop the offending one.
            continue;
        }
        if span.end > bytes.len() || span.start > span.end {
            return Err(AutoRedactError::Internal(format!(
                "malformed span: start={} end={} text_len={}",
                span.start,
                span.end,
                bytes.len()
            )));
        }
        if !text.is_char_boundary(cursor)
            || !text.is_char_boundary(span.start)
            || !text.is_char_boundary(span.end)
        {
            return Err(AutoRedactError::Internal(
                "PII span boundary is not a UTF-8 char boundary".to_string(),
            ));
        }
        out.push_str(&text[cursor..span.start]);
        let original = &text[span.start..span.end];
        let dummy = map.lookup_or_mint(&span.category, original, |c| haystack.contains(c));
        out.push_str(&dummy);
        cursor = span.end;
    }
    if cursor < bytes.len() {
        if !text.is_char_boundary(cursor) {
            return Err(AutoRedactError::Internal(
                "PII span tail boundary is not a UTF-8 char boundary".to_string(),
            ));
        }
        out.push_str(&text[cursor..]);
    }
    Ok(out)
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
            },
            Span {
                category: "private_email".into(),
                start: 27,
                end: 42,
            },
        ];
        let out = redact_one(text, &spans, &mut map, text).unwrap();
        assert_eq!(out, "Email redacted1@example.com or redacted2@example.com");
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
            },
            Span {
                category: "private_email".into(),
                start: 18,
                end: 29,
            },
        ];
        let out = redact_one(text, &spans, &mut map, text).unwrap();
        assert_eq!(
            out,
            "to redacted1@example.com or redacted1@example.com again"
        );
    }

    #[test]
    fn redact_one_fails_closed_on_non_char_boundary() {
        let mut map = RedactionMap::new();
        // "héllo" — `é` is 2 bytes (0xC3 0xA9). A span that ends inside
        // the multi-byte sequence must be rejected, not silently passed
        // through (which would leak the original text).
        let text = "héllo";
        let spans = vec![Span {
            category: "private_name".into(),
            start: 0,
            end: 2,
        }];
        let err = redact_one(text, &spans, &mut map, text).unwrap_err();
        assert!(
            matches!(err, super::super::AutoRedactError::Internal(_)),
            "expected Internal error on non-char boundary"
        );
    }

    #[test]
    fn redact_one_fails_closed_on_out_of_range_span() {
        let mut map = RedactionMap::new();
        let text = "short";
        let spans = vec![Span {
            category: "private_email".into(),
            start: 0,
            end: 999,
        }];
        let err = redact_one(text, &spans, &mut map, text).unwrap_err();
        assert!(matches!(err, super::super::AutoRedactError::Internal(_)));
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
            },
            Span {
                category: "private_name".into(),
                start: 2,
                end: 7,
            },
        ];
        let out = redact_one(text, &spans, &mut map, text).unwrap();
        assert_eq!(out, "Redacted001 world");
    }

    #[test]
    fn redact_one_merges_adjacent_same_category_spans() {
        // privacy-filter often returns two adjacent spans for one email
        // (e.g. local-part + ".com"). The result should be a single
        // minted dummy, not two.
        let mut map = RedactionMap::new();
        let text = "Email alice.chen@gmail.com today";
        let spans = vec![
            Span {
                category: "private_email".into(),
                start: 6,
                end: 22, // "alice.chen@gmail"
            },
            Span {
                category: "private_email".into(),
                start: 22,
                end: 26, // ".com"
            },
        ];
        let out = redact_one(text, &spans, &mut map, text).unwrap();
        assert_eq!(out, "Email redacted1@example.com today");
        assert_eq!(map.len(), 1, "merge must produce a single dummy");
    }

    #[test]
    fn redact_one_does_not_merge_different_categories() {
        let mut map = RedactionMap::new();
        // Use a UK-style phone so the original doesn't collide with our
        // minted `+1-555-01XX` dummy format and bump the ordinal.
        let text = "info: alice@x.com+44-20-7946-0958 stuff";
        let spans = vec![
            Span {
                category: "private_email".into(),
                start: 6,
                end: 17, // "alice@x.com"
            },
            Span {
                category: "private_phone".into(),
                start: 17,
                end: 33, // "+44-20-7946-0958"
            },
        ];
        let out = redact_one(text, &spans, &mut map, text).unwrap();
        // adjacent but DIFFERENT category — two distinct dummies, no merge
        assert_eq!(out, "info: redacted1@example.com+1-555-0100 stuff");
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn redact_one_does_not_merge_with_gap() {
        let mut map = RedactionMap::new();
        let text = "alice@x.com and bob@y.org";
        let spans = vec![
            Span {
                category: "private_email".into(),
                start: 0,
                end: 11,
            },
            Span {
                category: "private_email".into(),
                start: 16,
                end: 25,
            },
        ];
        let out = redact_one(text, &spans, &mut map, text).unwrap();
        assert_eq!(out, "redacted1@example.com and redacted2@example.com");
    }

    #[test]
    fn redact_one_empty_spans_passthrough() {
        let mut map = RedactionMap::new();
        let out = redact_one("nothing private", &[], &mut map, "nothing private").unwrap();
        assert_eq!(out, "nothing private");
        assert!(map.is_empty());
    }

    #[test]
    fn collect_walks_assistant_tool_call_arguments() {
        // Agent-loop scenario: an assistant message carries a tool_call
        // whose arguments JSON echoes the user's PII from a prior turn.
        // Without walking this, the original email re-leaks upstream on
        // every follow-up.
        let messages = vec![
            CompletionMessage {
                role: "user".to_string(),
                content: json!("Send a note to alice@example.com"),
                tool_call_id: None,
                tool_calls: None,
            },
            CompletionMessage {
                role: "assistant".to_string(),
                content: json!(null),
                tool_call_id: None,
                tool_calls: Some(vec![crate::completions::ports::CompletionToolCall {
                    id: "call_1".to_string(),
                    name: "send_email".to_string(),
                    arguments: r#"{"to":"alice@example.com"}"#.to_string(),
                    thought_signature: None,
                }]),
            },
        ];
        let (refs, texts) = collect_text_fragments(&messages);
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[0], "Send a note to alice@example.com");
        assert_eq!(texts[1], r#"{"to":"alice@example.com"}"#);
        assert!(matches!(refs[0], TextRef::Whole { msg_idx: 0 }));
        assert!(matches!(
            refs[1],
            TextRef::ToolCallArg {
                msg_idx: 1,
                tc_idx: 0
            }
        ));
    }

    #[test]
    fn write_back_updates_tool_call_arguments() {
        let mut messages = vec![CompletionMessage {
            role: "assistant".to_string(),
            content: json!(null),
            tool_call_id: None,
            tool_calls: Some(vec![crate::completions::ports::CompletionToolCall {
                id: "call_1".to_string(),
                name: "send_email".to_string(),
                arguments: r#"{"to":"alice@example.com"}"#.to_string(),
                thought_signature: None,
            }]),
        }];
        write_back(
            &mut messages,
            &[TextRef::ToolCallArg {
                msg_idx: 0,
                tc_idx: 0,
            }],
            vec![r#"{"to":"<email1>"}"#.to_string()],
        );
        let tcs = messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(tcs[0].arguments, r#"{"to":"<email1>"}"#);
    }
}
