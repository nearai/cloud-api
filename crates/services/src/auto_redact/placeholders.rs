use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Maximum byte length of any placeholder we mint. Bounds the streaming
/// unredact tail buffer so we never split a placeholder across SSE chunks.
///
/// `<account_number999>` is 19 bytes. We round up to 32 to leave headroom
/// for any future category prefix.
pub const MAX_PLACEHOLDER_LEN: usize = 32;

/// Matches any well-formed placeholder we could have minted: `<` + lowercase
/// letters/underscores + digits + `>`. Used by the unredact path to find
/// candidate replacements without allocating per chunk.
pub static PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<([a-z_]+)(\d+)>").expect("static regex is valid"));

/// Map a privacy-filter category string to the placeholder prefix used in
/// minted placeholders (e.g. `private_email` -> `email`). Unknown categories
/// fall back to the generic `pii` prefix; this keeps streaming unredact
/// matching simple while still distinguishing categories the model knows.
pub fn placeholder_prefix(category: &str) -> &'static str {
    match category {
        "private_email" => "email",
        "private_phone" => "phone",
        "account_number" => "account",
        "private_address" => "address",
        "private_name" => "name",
        _ => "pii",
    }
}

/// Bidirectional placeholder ↔ original mapping for a single redaction call.
///
/// - Same literal PII text within the call is deduplicated and reuses the
///   same placeholder.
/// - Ordinals are monotonic per category.
/// - If a candidate placeholder collides with a substring of the input,
///   the ordinal is bumped until unique. The set of "input literals to
///   avoid" is supplied at construction.
#[derive(Debug, Default, Clone)]
pub struct RedactionMap {
    placeholder_to_original: HashMap<String, String>,
    original_to_placeholder: HashMap<(String, String), String>,
    next_ordinal: HashMap<String, u32>,
    reserved_literals: std::collections::HashSet<String>,
}

impl RedactionMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark placeholders that already appear in the user's input so we never
    /// mint them ourselves. Pass the regex matches from each input text.
    pub fn reserve_literal(&mut self, placeholder: &str) {
        self.reserved_literals.insert(placeholder.to_string());
    }

    pub fn is_empty(&self) -> bool {
        self.placeholder_to_original.is_empty()
    }

    pub fn len(&self) -> usize {
        self.placeholder_to_original.len()
    }

    /// Return the placeholder for the given (category, original) — minting a
    /// new one if needed and skipping any ordinal that collides with a
    /// reserved literal.
    pub fn lookup_or_mint(&mut self, category: &str, original: &str) -> String {
        let prefix = placeholder_prefix(category);
        let key = (prefix.to_string(), original.to_string());
        if let Some(existing) = self.original_to_placeholder.get(&key) {
            return existing.clone();
        }

        let n = self.next_ordinal.entry(prefix.to_string()).or_insert(1);
        let mut placeholder = format!("<{prefix}{n}>");
        while self.reserved_literals.contains(&placeholder) {
            *n += 1;
            placeholder = format!("<{prefix}{n}>");
        }
        *n += 1;

        self.placeholder_to_original
            .insert(placeholder.clone(), original.to_string());
        self.original_to_placeholder
            .insert(key, placeholder.clone());
        placeholder
    }

    pub fn original_for(&self, placeholder: &str) -> Option<&str> {
        self.placeholder_to_original
            .get(placeholder)
            .map(String::as_str)
    }

    /// Replace every placeholder in `text` with its original, leaving any
    /// unknown placeholder-shaped tokens untouched (these are an LLM
    /// hallucinating a token we never minted).
    pub fn unredact(&self, text: &str) -> String {
        if self.is_empty() {
            return text.to_string();
        }
        PLACEHOLDER_RE
            .replace_all(text, |caps: &regex::Captures<'_>| {
                let whole = &caps[0];
                self.placeholder_to_original
                    .get(whole)
                    .cloned()
                    .unwrap_or_else(|| whole.to_string())
            })
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_known_categories() {
        assert_eq!(placeholder_prefix("private_email"), "email");
        assert_eq!(placeholder_prefix("private_phone"), "phone");
        assert_eq!(placeholder_prefix("account_number"), "account");
        assert_eq!(placeholder_prefix("anything_else"), "pii");
    }

    #[test]
    fn mint_monotonic_and_deduplicated() {
        let mut map = RedactionMap::new();
        let a1 = map.lookup_or_mint("private_email", "alice@example.com");
        let a2 = map.lookup_or_mint("private_email", "alice@example.com");
        let b = map.lookup_or_mint("private_email", "bob@example.com");
        assert_eq!(a1, "<email1>");
        assert_eq!(a2, "<email1>", "dedup: same original reuses placeholder");
        assert_eq!(b, "<email2>");
    }

    #[test]
    fn mint_skips_reserved_literals() {
        let mut map = RedactionMap::new();
        map.reserve_literal("<email1>");
        map.reserve_literal("<email2>");
        let first = map.lookup_or_mint("private_email", "alice@example.com");
        assert_eq!(
            first, "<email3>",
            "should skip reserved <email1> and <email2>"
        );
    }

    #[test]
    fn unredact_replaces_known_placeholders_only() {
        let mut map = RedactionMap::new();
        map.lookup_or_mint("private_email", "alice@example.com");
        let out = map.unredact("Send to <email1> and copy <email99> and <unknown1>.");
        assert_eq!(
            out, "Send to alice@example.com and copy <email99> and <unknown1>.",
            "<email99> was never minted; <unknown1> uses a category we never produced"
        );
    }

    #[test]
    fn unredact_empty_map_returns_input_unchanged() {
        let map = RedactionMap::new();
        let s = "no placeholders here <email1> still no";
        assert_eq!(map.unredact(s), s);
    }

    #[test]
    fn placeholder_re_matches_expected_shapes() {
        let m: Vec<&str> = PLACEHOLDER_RE
            .find_iter("hi <email1> and <account_number2> and <bad-1> and < email3 >")
            .map(|m| m.as_str())
            .collect();
        assert_eq!(m, vec!["<email1>", "<account_number2>"]);
    }

    #[test]
    fn unredact_handles_adjacent_placeholders() {
        let mut map = RedactionMap::new();
        map.lookup_or_mint("private_email", "a@b.com");
        map.lookup_or_mint("private_phone", "+1-555-0100");
        let out = map.unredact("<email1><phone1>");
        assert_eq!(out, "a@b.com+1-555-0100");
    }
}
