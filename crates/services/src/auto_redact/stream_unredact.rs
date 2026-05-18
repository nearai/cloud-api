//! Sliding-window stream unredact for natural-dummy placeholders.
//!
//! The model emits text fragments over time. Some fragments contain a
//! complete dummy we minted; some contain only a prefix (the rest is
//! coming in the next chunk). We must:
//!
//! 1. Substitute every complete dummy in the emitted text.
//! 2. Hold back any text that could still grow into a dummy until the
//!    next chunk arrives.
//!
//! Strategy:
//! - Maintain a `tail` buffer carrying over from the previous chunk.
//! - On each `process(chunk)`: combine `tail + chunk`, run
//!   `map.unredact` (longest-first substring substitution), then split:
//!   emit everything except the last `max_dummy_len` bytes, hold the
//!   rest as the new tail.
//! - This guarantees any partial dummy at the end of the stream is
//!   either completed before emit, or held until `flush`.
//!
//! On `flush`, the remaining tail is run through `unredact` once more
//! (in case a complete dummy lived entirely inside the tail) and emitted.

use super::placeholders::{RedactionMap, MAX_PLACEHOLDER_LEN};

/// Per-stream unredacter. One instance covers a single (choice, field)
/// stream — e.g. choice 0's `content` deltas.
#[derive(Debug)]
pub struct StreamUnredact {
    map: std::sync::Arc<RedactionMap>,
    tail: String,
    /// When true, replacements are JSON-escaped before substitution.
    /// Used for `tool_calls[*].function.arguments` whose emitted bytes
    /// are inside a JSON string literal.
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

    /// Variant for streams whose emitted text is the body of a JSON
    /// string literal (e.g. tool-call arguments).
    pub fn new_for_json_string(map: std::sync::Arc<RedactionMap>) -> Self {
        Self {
            map,
            tail: String::new(),
            json_escape: true,
        }
    }

    /// True if the underlying map is empty — short-circuit to
    /// passthrough.
    pub fn is_noop(&self) -> bool {
        self.map.is_empty()
    }

    fn substitute(&self, s: &str) -> String {
        if self.json_escape {
            self.map.unredact_json_string(s)
        } else {
            self.map.unredact(s)
        }
    }

    /// Effective hold size: at least the longest minted dummy, capped
    /// by `MAX_PLACEHOLDER_LEN` to bound memory.
    fn hold_size(&self) -> usize {
        self.map.max_dummy_len().min(MAX_PLACEHOLDER_LEN)
    }

    /// Process the next chunk of text. Returns the prefix that is safe
    /// to emit; any text that could still be part of an unfinished
    /// dummy is kept in `self.tail`.
    pub fn process(&mut self, chunk: &str) -> String {
        if self.map.is_empty() {
            // Hot path: nothing to replace, don't even buffer.
            return chunk.to_string();
        }

        let mut buf = std::mem::take(&mut self.tail);
        buf.push_str(chunk);

        let substituted = self.substitute(&buf);
        let hold = self.hold_size();
        if substituted.len() <= hold {
            self.tail = substituted;
            return String::new();
        }

        // Split at a char boundary `hold` bytes from the end.
        let mut split_at = substituted.len() - hold;
        while !substituted.is_char_boundary(split_at) {
            split_at -= 1;
        }
        self.tail = substituted[split_at..].to_string();
        substituted[..split_at].to_string()
    }

    /// Emit any pending tail at end of stream. Unknown dummy-shaped
    /// tokens are left literal (signal that the upstream truncated
    /// mid-dummy or hallucinated one we never minted).
    pub fn flush(mut self) -> String {
        if self.tail.is_empty() {
            return String::new();
        }
        let tail = std::mem::take(&mut self.tail);
        self.substitute(&tail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn map_with(entries: &[(&str, &str)]) -> Arc<RedactionMap> {
        let mut m = RedactionMap::new();
        for (cat, val) in entries {
            m.lookup_or_mint(cat, val, |_| false);
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
        let map = map_with(&[("private_email", "alice@b.com")]);
        let mut u = StreamUnredact::new(map);
        // The minted dummy is "redacted1@example.com".
        let out = u.process("Email: redacted1@example.com!");
        let tail = u.flush();
        assert_eq!(out + &tail, "Email: alice@b.com!");
    }

    #[test]
    fn split_dummy_across_two_chunks() {
        let map = map_with(&[("private_email", "alice@example.com")]);
        let mut u = StreamUnredact::new(map);
        // Stream "Email: redacted1@example.com!" split mid-dummy.
        let part1 = u.process("Email: redacte");
        // The "redacte" partial must be held — not emitted yet.
        assert!(
            !part1.contains("redacte"),
            "partial dummy must not leak before completion: {part1:?}"
        );
        let part2 = u.process("d1@example.com!");
        let tail = u.flush();
        assert_eq!(part1 + &part2 + &tail, "Email: alice@example.com!");
    }

    #[test]
    fn split_dummy_across_many_chunks() {
        let map = map_with(&[("private_email", "x@y.z")]);
        let mut u = StreamUnredact::new(map);
        let mut acc = String::new();
        // Stream the dummy char-by-char.
        for ch in "redacted1@example.com".chars() {
            acc.push_str(&u.process(&ch.to_string()));
        }
        acc.push_str(&u.flush());
        assert_eq!(acc, "x@y.z");
    }

    #[test]
    fn multiple_dummies_in_one_chunk() {
        let map = map_with(&[
            ("private_email", "alice@x"),
            ("private_phone", "+1-555-0900"),
        ]);
        let mut u = StreamUnredact::new(map);
        // Dummies: redacted1@example.com, +1-555-0100
        let out = u.process("Call +1-555-0100 at redacted1@example.com.") + &u.flush();
        assert_eq!(out, "Call +1-555-0900 at alice@x.");
    }

    #[test]
    fn unknown_dummy_passes_through_literally() {
        let map = map_with(&[("private_email", "real@example.org")]);
        let mut u = StreamUnredact::new(map);
        // `redacted42@example.com` was never minted.
        let out = u.process("hi redacted42@example.com there") + &u.flush();
        assert_eq!(out, "hi redacted42@example.com there");
    }

    #[test]
    fn long_run_without_potential_dummy_emits_immediately() {
        let map = map_with(&[("private_email", "x@y.z")]);
        let mut u = StreamUnredact::new(map);
        let payload = "a".repeat(1024);
        let out = u.process(&payload) + &u.flush();
        assert_eq!(out, payload);
    }

    #[test]
    fn utf8_multibyte_safe() {
        let map = map_with(&[("private_email", "x@y.z")]);
        let mut u = StreamUnredact::new(map);
        let out = u.process("héllo redacte") + &u.process("d1@example.com!") + &u.flush();
        assert_eq!(out, "héllo x@y.z!");
    }

    #[test]
    fn json_string_variant_escapes_quote_in_replacement() {
        let map = map_with(&[("private_name", r#"Patrick O"Brien"#)]);
        let mut u = StreamUnredact::new_for_json_string(map);
        // private_name dummy is Redacted001
        let args = r#"{"to":"Redacted001"}"#;
        let out = u.process(args) + &u.flush();
        assert_eq!(out, r#"{"to":"Patrick O\"Brien"}"#);
        let _: serde_json::Value = serde_json::from_str(&out).unwrap();
    }

    #[test]
    fn json_string_variant_escapes_across_chunk_split() {
        let map = map_with(&[("private_name", r#"O"X"#)]);
        let mut u = StreamUnredact::new_for_json_string(map);
        let p1 = u.process(r#"{"n":"Redact"#);
        let p2 = u.process(r#"ed001"}"#);
        let tail = u.flush();
        let out = p1 + &p2 + &tail;
        assert_eq!(out, r#"{"n":"O\"X"}"#);
        let _: serde_json::Value = serde_json::from_str(&out).unwrap();
    }
}
