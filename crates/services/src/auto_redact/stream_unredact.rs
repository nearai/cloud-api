//! Sliding-window stream unredact.
//!
//! Replaces placeholders in a stream of text chunks while never splitting a
//! placeholder across emitted boundaries. The state machine holds back a
//! short tail of pending text — at most `MAX_PLACEHOLDER_LEN` bytes — so an
//! incomplete `<emailN>`-shaped token never escapes prematurely.

use super::placeholders::{RedactionMap, MAX_PLACEHOLDER_LEN};

/// Per-stream unredacter. One instance covers a single completion stream.
#[derive(Debug)]
pub struct StreamUnredact {
    map: std::sync::Arc<RedactionMap>,
    tail: String,
    /// When true, replacements are JSON-escaped before substitution. Used
    /// for streaming `tool_calls[*].function.arguments` where the emitted
    /// chars are inside a JSON string literal.
    json_escape: bool,
}

impl StreamUnredact {
    pub fn new(map: std::sync::Arc<RedactionMap>) -> Self {
        Self {
            map,
            tail: String::new(),
            json_escape: false,
        }
    }

    /// Variant for streams whose emitted text is the body of a JSON string
    /// literal (e.g. tool-call arguments). Replacements are JSON-escaped so
    /// originals containing `"`, `\`, control chars, or non-ASCII never
    /// corrupt the surrounding JSON.
    pub fn new_for_json_string(map: std::sync::Arc<RedactionMap>) -> Self {
        Self {
            map,
            tail: String::new(),
            json_escape: true,
        }
    }

    /// True if the underlying map has no minted placeholders. Callers can
    /// short-circuit and skip wrapping the stream entirely.
    pub fn is_noop(&self) -> bool {
        self.map.is_empty()
    }

    /// Process the next chunk of text. Returns the prefix that is safe to
    /// emit; any text that could be the start of an unfinished placeholder
    /// is kept in the tail until the next call (or until `flush`).
    pub fn process(&mut self, chunk: &str) -> String {
        if self.map.is_empty() {
            // Hot path: nothing to replace. Don't even buffer.
            return chunk.to_string();
        }

        // Combine carryover tail with new chunk. We work on the combined
        // buffer so a placeholder that straddles a chunk boundary is matched
        // as a single token.
        let mut buf = std::mem::take(&mut self.tail);
        buf.push_str(chunk);

        let split_at = safe_emit_boundary(&buf);
        let hold = buf.split_off(split_at);
        let emit = buf;
        self.tail = hold;

        if self.json_escape {
            self.map.unredact_json_string(&emit)
        } else {
            self.map.unredact(&emit)
        }
    }

    /// Emit any pending tail at end of stream. Unmatched placeholder-shaped
    /// tokens are left as literal text (signal that the LLM hallucinated a
    /// token we never minted).
    pub fn flush(mut self) -> String {
        if self.tail.is_empty() {
            return String::new();
        }
        let tail = std::mem::take(&mut self.tail);
        if self.json_escape {
            self.map.unredact_json_string(&tail)
        } else {
            self.map.unredact(&tail)
        }
    }
}

/// Decide where to split the buffer between "emit now" and "hold for next
/// chunk." The hold region must include any `<` that could still grow into
/// a complete `<categoryN>` token.
///
/// Strategy: walk the rightmost `MAX_PLACEHOLDER_LEN` bytes and find the
/// earliest `<` that has no `>` after it. Everything from that `<` onward
/// must be held. If no such `<` exists, the entire buffer is safe to emit.
fn safe_emit_boundary(buf: &str) -> usize {
    let bytes = buf.as_bytes();
    let n = bytes.len();
    if n == 0 {
        return 0;
    }
    let scan_from = n.saturating_sub(MAX_PLACEHOLDER_LEN);

    let mut emit_until = n;
    for i in scan_from..n {
        if bytes[i] == b'<' {
            // Is there a `>` somewhere after i?
            let has_closing = bytes[i + 1..].contains(&b'>');
            if !has_closing {
                emit_until = i;
                break;
            }
        }
    }
    // `<` is ASCII, so any byte offset we choose at a `<` is a char boundary.
    emit_until
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn map_with(entries: &[(&str, &str)]) -> Arc<RedactionMap> {
        let mut m = RedactionMap::new();
        for (cat, val) in entries {
            m.lookup_or_mint(cat, val);
        }
        Arc::new(m)
    }

    #[test]
    fn passthrough_when_map_empty() {
        let map = Arc::new(RedactionMap::new());
        let mut u = StreamUnredact::new(map);
        assert!(u.is_noop());
        let s = u.process("hello world");
        assert_eq!(s, "hello world");
        assert_eq!(u.flush(), "");
    }

    #[test]
    fn single_chunk_replacement() {
        let map = map_with(&[("private_email", "a@b.com")]);
        let mut u = StreamUnredact::new(map);
        // <email1> is the placeholder for "a@b.com"
        let out = u.process("Email: <email1>!");
        let tail = u.flush();
        assert_eq!(out + &tail, "Email: a@b.com!");
    }

    #[test]
    fn split_placeholder_across_two_chunks() {
        let map = map_with(&[("private_email", "alice@example.com")]);
        let mut u = StreamUnredact::new(map);
        let part1 = u.process("Email: <emai");
        // The "<emai" must be held — nothing emitted from that point on.
        assert_eq!(part1, "Email: ");
        let part2 = u.process("l1>!");
        // Once the placeholder is complete, full replacement.
        let tail = u.flush();
        assert_eq!(part1 + &part2 + &tail, "Email: alice@example.com!");
    }

    #[test]
    fn split_placeholder_across_many_chunks() {
        let map = map_with(&[("private_email", "x@y.z")]);
        let mut u = StreamUnredact::new(map);
        let mut acc = String::new();
        for ch in "<email1>".chars() {
            acc.push_str(&u.process(&ch.to_string()));
        }
        acc.push_str(&u.flush());
        assert_eq!(acc, "x@y.z");
    }

    #[test]
    fn multiple_placeholders_in_one_chunk() {
        let map = map_with(&[
            ("private_email", "a@b.com"),
            ("private_phone", "+1-555-0100"),
        ]);
        let mut u = StreamUnredact::new(map);
        let out = u.process("Call <phone1> at <email1>.") + &u.flush();
        assert_eq!(out, "Call +1-555-0100 at a@b.com.");
    }

    #[test]
    fn hallucinated_placeholder_passes_through_literally() {
        let map = map_with(&[("private_email", "real@example.com")]);
        let mut u = StreamUnredact::new(map);
        // <email42> was never minted: appears as-is.
        let out = u.process("hi <email42> there") + &u.flush();
        assert_eq!(out, "hi <email42> there");
    }

    #[test]
    fn dangling_open_angle_at_end_is_held_then_flushed_literally() {
        let map = map_with(&[("private_email", "x@y.z")]);
        let mut u = StreamUnredact::new(map);
        let out = u.process("see this <") + &u.flush();
        assert_eq!(out, "see this <");
    }

    #[test]
    fn long_run_without_open_angle_emits_immediately() {
        let map = map_with(&[("private_email", "x@y.z")]);
        let mut u = StreamUnredact::new(map);
        let payload = "a".repeat(1024);
        let out = u.process(&payload);
        assert_eq!(out, payload, "no '<' means no hold");
    }

    #[test]
    fn adjacent_placeholders_split_at_boundary() {
        let map = map_with(&[("private_email", "a@b.com"), ("private_phone", "+1-0")]);
        let mut u = StreamUnredact::new(map);
        // Split in the middle of "<email1><phone1>"
        let a = u.process("<email1");
        let b = u.process("><phone1>");
        let flush = u.flush();
        assert_eq!(a + &b + &flush, "a@b.com+1-0");
    }

    #[test]
    fn utf8_multibyte_safe() {
        // Build a chunk that ends in a multibyte char before a held `<`.
        let map = map_with(&[("private_email", "x@y.z")]);
        let mut u = StreamUnredact::new(map);
        let out = u.process("héllo <emai") + &u.process("l1>!") + &u.flush();
        assert_eq!(out, "héllo x@y.z!");
    }

    #[test]
    fn json_string_variant_escapes_quote_in_replacement() {
        // PII original contains a literal `"`. Substituting into a JSON
        // string body context must escape it to `\"` or the surrounding
        // JSON corrupts.
        let map = map_with(&[("private_name", r#"Patrick O"Brien"#)]);
        let mut u = StreamUnredact::new_for_json_string(map);
        let args = r#"{"to":"<name1>"}"#;
        let out = u.process(args) + &u.flush();
        assert_eq!(out, r#"{"to":"Patrick O\"Brien"}"#);
        // Must round-trip parse.
        let _: serde_json::Value = serde_json::from_str(&out).unwrap();
    }

    #[test]
    fn json_string_variant_escapes_across_chunk_split() {
        let map = map_with(&[("private_name", r#"O"X"#)]);
        let mut u = StreamUnredact::new_for_json_string(map);
        let p1 = u.process(r#"{"n":"<nam"#);
        let p2 = u.process(r#"e1>"}"#);
        let tail = u.flush();
        let out = p1 + &p2 + &tail;
        assert_eq!(out, r#"{"n":"O\"X"}"#);
        let _: serde_json::Value = serde_json::from_str(&out).unwrap();
    }

    #[test]
    fn json_string_variant_no_op_for_simple_pii() {
        // Plain-ASCII PII should produce identical output to the regular
        // unredact path.
        let map = map_with(&[("private_email", "alice@example.com")]);
        let mut a = StreamUnredact::new(map.clone());
        let mut b = StreamUnredact::new_for_json_string(map);
        let s = r#"{"to":"<email1>"}"#;
        let out_a = a.process(s) + &a.flush();
        let out_b = b.process(s) + &b.flush();
        assert_eq!(out_a, out_b);
    }

    #[test]
    fn long_held_buffer_eventually_releases() {
        // If the held region grows past MAX_PLACEHOLDER_LEN with no `>`,
        // the leading `<` is past the scan window and gets emitted.
        let map = map_with(&[("private_email", "x@y.z")]);
        let mut u = StreamUnredact::new(map);
        // A `<` followed by lots of non-`>` chars — eventually we have to
        // release it because no real placeholder is this long.
        let first = u.process(&format!("<{}", "a".repeat(MAX_PLACEHOLDER_LEN * 2)));
        // The `<` and the leading run of a's are emitted; the tail holds
        // only the last MAX_PLACEHOLDER_LEN bytes (still no `>`).
        let flushed = u.flush();
        let combined = first + &flushed;
        assert!(combined.starts_with('<'));
        assert!(combined.contains('a'));
    }
}
