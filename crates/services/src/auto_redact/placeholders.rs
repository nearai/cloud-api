//! Placeholder minting and un-redact substitution.
//!
//! We replace each PII span with a **realistic-looking dummy** in the
//! shape of the underlying category (e.g. emails get `redacted{N}@example.com`,
//! phones get `+1-555-01{NN:02}`). The model receives values that look
//! like real data, so it doesn't trigger template-aware behavior
//! (refusals, bracket-stripping, paraphrasing) the way the prior
//! `<emailN>`-style placeholders did.
//!
//! Un-redact is exact-string substitution iterating the minted set
//! longest-first to avoid prefix collisions. No regex.
//!
//! Properties:
//! - Same `(category, original)` re-uses the same dummy (dedup).
//! - Ordinals are monotonic per category.
//! - Minting refuses any dummy that already appears as a substring of
//!   the request input (collision avoidance).

use std::collections::HashMap;

/// Maximum byte length of any placeholder we mint. Bounds the streaming
/// unredact tail buffer. Formats today (with ordinals up to ~10^6):
/// `redacted_secret_999999` = 22 bytes; `redacted1000000@example.com` = 27;
/// `100000 Redacted Way` = 19. Rounded up generously.
pub const MAX_PLACEHOLDER_LEN: usize = 64;

/// Mint a category-shaped dummy for a given ordinal. Dummies are picked
/// from RFC-reserved or invalid ranges (`example.com`, `555-01XX`, SSN
/// area `000`) so they cannot collide with real third-party PII.
fn mint_dummy(category: &str, ordinal: u32) -> String {
    match category {
        "private_email" => format!("redacted{ordinal}@example.com"),
        "private_phone" => {
            // 555-0100 through 555-0199 is RFC 2606 fictional. We
            // extend by stepping the third digit when ordinals exceed
            // 99 — still in the `555-0XXX` unassigned space.
            let hundreds = (ordinal - 1) / 100 + 1;
            let suffix = (ordinal - 1) % 100;
            format!("+1-555-0{hundreds}{suffix:02}")
        }
        "account_number" => format!("000-00-{ordinal:04}"),
        "private_address" => format!("{ordinal}00 Redacted Way"),
        "private_name" => format!("Redacted{ordinal:03}"),
        "secret" => format!("redacted_secret_{ordinal:06}"),
        // Any unknown category falls back to a generic, recognizable form.
        _ => format!("redacted_pii_{ordinal}"),
    }
}

/// Bidirectional placeholder ↔ original mapping for a single request.
#[derive(Debug, Default, Clone)]
pub struct RedactionMap {
    /// Sorted by descending dummy length so un-redact substitutes
    /// longest first (avoids one dummy being a prefix of another).
    entries: Vec<(String, String)>,
    /// Fast lookup for dedup: same `(category, original)` reuses dummy.
    original_to_dummy: HashMap<(String, String), String>,
    /// Per-category ordinal counter, monotonic for stable test output.
    next_ordinal: HashMap<String, u32>,
}

impl RedactionMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Length of the longest minted dummy, in bytes. Streaming un-redact
    /// uses this to size its sliding tail buffer.
    pub fn max_dummy_len(&self) -> usize {
        self.entries.first().map(|(d, _)| d.len()).unwrap_or(0)
    }

    /// Return the existing dummy for `(category, original)` or mint a
    /// fresh one. `would_collide(d)` is called for each candidate dummy
    /// and must return true if `d` appears anywhere in the request
    /// (haystack) the caller wants us to avoid. The minted ordinal is
    /// advanced until a non-colliding candidate is found.
    pub fn lookup_or_mint(
        &mut self,
        category: &str,
        original: &str,
        would_collide: impl Fn(&str) -> bool,
    ) -> String {
        let key = (category.to_string(), original.to_string());
        if let Some(existing) = self.original_to_dummy.get(&key) {
            return existing.clone();
        }

        let dummy = loop {
            let n_ref = self.next_ordinal.entry(category.to_string()).or_insert(1);
            let n = *n_ref;
            *n_ref += 1;
            let candidate = mint_dummy(category, n);
            // Don't mint a dummy that collides with input text, an
            // existing dummy, or a reserved literal. Loop until clean.
            if would_collide(&candidate) || self.entries.iter().any(|(d, _)| d == &candidate) {
                continue;
            }
            break candidate;
        };

        // Insert sorted by descending length so unredact iterates
        // longest-first (handles `redacted_secret_1` containing
        // `redacted_pii_1` as a substring, etc.).
        let pos = self.entries.partition_point(|(d, _)| d.len() > dummy.len());
        self.entries
            .insert(pos, (dummy.clone(), original.to_string()));
        self.original_to_dummy.insert(key, dummy.clone());
        dummy
    }

    /// Replace every minted dummy in `text` with its original. Iterates
    /// longest-first to handle nested-prefix cases safely. Unknown
    /// dummy-shaped strings (we never minted) pass through literally.
    pub fn unredact(&self, text: &str) -> String {
        if self.entries.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for (dummy, original) in &self.entries {
            if out.contains(dummy.as_str()) {
                out = out.replace(dummy.as_str(), original);
            }
        }
        out
    }

    /// Like [`unredact`], but when the text being substituted into is
    /// itself a JSON-encoded string (e.g. tool_call arguments). The
    /// replacement is JSON-escaped before insertion so a PII original
    /// containing `"`, `\`, control chars, or non-ASCII never corrupts
    /// the surrounding JSON.
    pub fn unredact_json_string(&self, text: &str) -> String {
        if self.entries.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for (dummy, original) in &self.entries {
            if out.contains(dummy.as_str()) {
                let escaped = json_escape_inner(original);
                out = out.replace(dummy.as_str(), &escaped);
            }
        }
        out
    }
}

/// JSON-escape a string for embedding inside a JSON string literal —
/// the chars between the surrounding `"..."` quotes. Returns the body
/// only. Uses `serde_json::to_string` then strips the outer quotes.
fn json_escape_inner(s: &str) -> String {
    match serde_json::to_string(s) {
        Ok(quoted) if quoted.len() >= 2 => quoted[1..quoted.len() - 1].to_string(),
        _ => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never_collides(_: &str) -> bool {
        false
    }

    #[test]
    fn mint_email_format() {
        let mut m = RedactionMap::new();
        let d = m.lookup_or_mint("private_email", "alice@x.com", never_collides);
        assert_eq!(d, "redacted1@example.com");
        let d2 = m.lookup_or_mint("private_email", "bob@y.com", never_collides);
        assert_eq!(d2, "redacted2@example.com");
    }

    #[test]
    fn mint_phone_format() {
        let mut m = RedactionMap::new();
        assert_eq!(
            m.lookup_or_mint("private_phone", "+1-555-0100", never_collides),
            "+1-555-0100"
        );
        assert_eq!(
            m.lookup_or_mint("private_phone", "+1-555-0199", never_collides),
            "+1-555-0101"
        );
    }

    #[test]
    fn mint_account_format() {
        let mut m = RedactionMap::new();
        assert_eq!(
            m.lookup_or_mint("account_number", "412-23-4567", never_collides),
            "000-00-0001"
        );
    }

    #[test]
    fn mint_secret_format() {
        let mut m = RedactionMap::new();
        assert_eq!(
            m.lookup_or_mint("secret", "sk_abc", never_collides),
            "redacted_secret_000001"
        );
    }

    #[test]
    fn mint_dedup_same_original() {
        let mut m = RedactionMap::new();
        let d1 = m.lookup_or_mint("private_email", "a@b.com", never_collides);
        let d2 = m.lookup_or_mint("private_email", "a@b.com", never_collides);
        assert_eq!(d1, d2, "same (category, original) must reuse the dummy");
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn mint_skips_colliding_candidates() {
        let mut m = RedactionMap::new();
        // Pretend ordinals 1 and 2 already appear in input text.
        let banned = ["redacted1@example.com", "redacted2@example.com"];
        let d = m.lookup_or_mint("private_email", "alice@x.com", |c| banned.contains(&c));
        assert_eq!(d, "redacted3@example.com");
    }

    #[test]
    fn unknown_category_falls_back_to_generic_pii() {
        let mut m = RedactionMap::new();
        let d = m.lookup_or_mint("some_new_category", "X", never_collides);
        assert_eq!(d, "redacted_pii_1");
    }

    #[test]
    fn unredact_basic() {
        let mut m = RedactionMap::new();
        let d = m.lookup_or_mint("private_email", "alice@x.com", never_collides);
        let out = m.unredact(&format!("Email is {d}!"));
        assert_eq!(out, "Email is alice@x.com!");
    }

    #[test]
    fn unredact_longest_first() {
        // `redacted_pii_1` and `redacted_pii_10` both minted: the second
        // contains the first as a prefix. Naive replace order could
        // double-substitute. Verify longest-first.
        let mut m = RedactionMap::new();
        m.lookup_or_mint("unknown", "A", never_collides); // -> redacted_pii_1
        for _ in 2..=10 {
            m.lookup_or_mint("unknown", &format!("X{}", m.len()), never_collides);
        }
        let last = m.lookup_or_mint("unknown", "TEN", never_collides);
        assert_eq!(last, "redacted_pii_11");
        // Substituting "redacted_pii_11 and redacted_pii_1" must not
        // turn the longer into "A1".
        let out = m.unredact("see redacted_pii_11 and redacted_pii_1.");
        assert_eq!(out, "see TEN and A.");
    }

    #[test]
    fn unredact_unknown_dummy_passes_through() {
        let mut m = RedactionMap::new();
        m.lookup_or_mint("private_email", "alice@x.com", never_collides);
        // `redacted999@example.com` was never minted — left literal.
        let out = m.unredact("see redacted999@example.com");
        assert_eq!(out, "see redacted999@example.com");
    }

    #[test]
    fn unredact_json_string_escapes_quote() {
        let mut m = RedactionMap::new();
        let d = m.lookup_or_mint("private_name", r#"Patrick O"Brien"#, never_collides);
        let out = m.unredact_json_string(&format!(r#"{{"to":"{d}"}}"#));
        assert_eq!(out, r#"{"to":"Patrick O\"Brien"}"#);
        let _: serde_json::Value = serde_json::from_str(&out).unwrap();
    }

    #[test]
    fn unredact_json_string_no_op_for_simple_pii() {
        let mut m = RedactionMap::new();
        let d = m.lookup_or_mint("private_email", "alice@example.com", never_collides);
        let body = format!(r#"{{"to":"{d}"}}"#);
        assert_eq!(
            m.unredact(&body),
            m.unredact_json_string(&body),
            "json variant matches plain when no special chars"
        );
    }

    #[test]
    fn max_dummy_len_tracks_longest() {
        let mut m = RedactionMap::new();
        m.lookup_or_mint("private_email", "a", never_collides); // 21
        m.lookup_or_mint("account_number", "x", never_collides); // 11
        m.lookup_or_mint("secret", "y", never_collides); // 22
                                                         // longest is secret format: `redacted_secret_000001` = 22 bytes
        assert_eq!(m.max_dummy_len(), "redacted_secret_000001".len());
    }
}
