//! Per-request placement affinity key derivation.
//!
//! [`derive`] turns request-scoped, low-cardinality signals (a client-supplied
//! session/cache hint, or — absent one — the text of the first system/developer
//! and first user message) into a [`placement::affinity::AffinityKey`] the
//! `placement` crate uses for warm-cache routing.
//!
//! Privacy: the inputs here are customer content (session identifiers, prompt
//! text). The derived key is an opaque, unlinkable-by-inspection HMAC output
//! (see `placement::affinity::AffinityKey`, which has no `Debug`/`Display`/
//! `Serialize`). Never log the key, its hex encoding, or the raw inputs.

use std::collections::HashMap;

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use inference_providers::{ChatMessage, MessageRole};
use placement::affinity::AffinityKey;
use placement::decision::AffinitySource;

type HmacSha256 = Hmac<Sha256>;

/// Every candidate value (header hint, `session_id`, `prompt_cache_key`, and
/// the constructed prefix buffer) is truncated to this many bytes before
/// hashing.
const MAX_VALUE_BYTES: usize = 256;

/// `params.extra` key carrying the derived affinity key, lowercase hex of
/// the 16 key bytes. Stripped before the upstream provider request, the
/// same way `x_model_pub_key` is.
pub const AFFINITY_EXTRA_KEY: &str = "x_placement_affinity";
/// `params.extra` key carrying the affinity key's source (`"client"` or
/// `"prefix"`). Stripped before the upstream provider request alongside
/// [`AFFINITY_EXTRA_KEY`].
pub const AFFINITY_SOURCE_EXTRA_KEY: &str = "x_placement_affinity_source";

/// Derive a per-request [`AffinityKey`] plus the [`AffinitySource`] it came
/// from, or `None` when no usable signal exists.
///
/// Order of precedence:
/// 1. `session_hint` — the `x-session-id` request header.
/// 2. `body_extra["session_id"]` — a string value only; a missing key, a
///    non-string value, or an empty string all count as absent.
/// 3. `body_extra["prompt_cache_key"]` — same rules as above.
/// 4. When `!is_e2ee`: the role and text of the first system/developer
///    message plus the first user message (already-flattened content: for
///    an array of content parts, only `type: "text"` parts are used, in
///    order; image/audio parts are skipped). `None` if neither message has
///    any text.
/// 5. Otherwise (E2EE and no client-supplied key): `None`.
///
/// Every candidate value is truncated to [`MAX_VALUE_BYTES`] bytes (on a
/// byte boundary — this hashes raw bytes, so UTF-8 boundaries don't matter)
/// before it is hashed as `HMAC-SHA256(secret, org_id ‖ 0 ‖ model ‖ 0 ‖
/// source ‖ 0 ‖ value)`, truncated to the leading 16 bytes.
pub fn derive(
    org_id: &str,
    model: &str,
    session_hint: Option<&str>,
    body_extra: &HashMap<String, serde_json::Value>,
    messages: &[ChatMessage],
    is_e2ee: bool,
    secret: &[u8; 32],
) -> Option<(AffinityKey, AffinitySource)> {
    if let Some(hint) = non_empty(session_hint) {
        return Some((
            hashed_key(secret, org_id, model, "client", truncate(hint.as_bytes())),
            AffinitySource::Client,
        ));
    }

    if let Some(session_id) = extra_str(body_extra, "session_id") {
        return Some((
            hashed_key(
                secret,
                org_id,
                model,
                "client",
                truncate(session_id.as_bytes()),
            ),
            AffinitySource::Client,
        ));
    }

    if let Some(prompt_cache_key) = extra_str(body_extra, "prompt_cache_key") {
        return Some((
            hashed_key(
                secret,
                org_id,
                model,
                "client",
                truncate(prompt_cache_key.as_bytes()),
            ),
            AffinitySource::Client,
        ));
    }

    if !is_e2ee {
        if let Some(prefix_bytes) = prefix_value(messages) {
            return Some((
                hashed_key(secret, org_id, model, "prefix", &prefix_bytes),
                AffinitySource::Prefix,
            ));
        }
    }

    None
}

/// `Some(s)` only when `s` is non-empty.
fn non_empty(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.is_empty())
}

/// Read `extra[key]` as a non-empty string. A missing key, a non-string
/// value, and an empty string all count as absent (per the task's rules).
fn extra_str<'a>(extra: &'a HashMap<String, serde_json::Value>, key: &str) -> Option<&'a str> {
    extra
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Concatenate the `type: "text"` parts of a message's content, in order.
/// A plain string content is used as-is. Image/audio parts (and any other
/// non-text part) are skipped. Returns an empty string for absent content.
fn message_text(content: &Option<serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                        out.push_str(text);
                    }
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// Build the unambiguous, length-prefixed byte buffer for the prefix
/// source: `role1 ‖ text1 ‖ role2 ‖ text2`, each field prefixed by its
/// big-endian `u32` byte length. `role1`/`text1` are the first
/// system-role message (developer messages are already normalized to
/// `MessageRole::System` upstream of this call); `role2`/`text2` are the
/// first user-role message. A missing message contributes an empty role
/// and empty text. Returns `None` when both texts are empty — there is
/// nothing to key on.
///
/// The system and user texts are each truncated to [`MAX_VALUE_BYTES`]
/// bytes *independently*, before framing — not the composed buffer as a
/// whole. Truncating the whole buffer instead would let a long shared
/// system prompt push the user message's bytes out of the truncation
/// window entirely, so every conversation sharing that system prompt would
/// collapse onto the same key (a routing hotspot) regardless of who the
/// user is.
fn prefix_value(messages: &[ChatMessage]) -> Option<Vec<u8>> {
    let system_msg = messages.iter().find(|m| m.role == MessageRole::System);
    let user_msg = messages.iter().find(|m| m.role == MessageRole::User);

    let system_text = system_msg
        .map(|m| message_text(&m.content))
        .unwrap_or_default();
    let user_text = user_msg
        .map(|m| message_text(&m.content))
        .unwrap_or_default();

    if system_text.is_empty() && user_text.is_empty() {
        return None;
    }

    let system_role = if system_msg.is_some() { "system" } else { "" };
    let user_role = if user_msg.is_some() { "user" } else { "" };

    let mut buf = Vec::new();
    for (role, text) in [
        (system_role, system_text.as_str()),
        (user_role, user_text.as_str()),
    ] {
        write_length_prefixed(&mut buf, role.as_bytes());
        write_length_prefixed(&mut buf, truncate(text.as_bytes()));
    }
    Some(buf)
}

fn write_length_prefixed(buf: &mut Vec<u8>, field: &[u8]) {
    buf.extend_from_slice(&(field.len() as u32).to_be_bytes());
    buf.extend_from_slice(field);
}

/// Truncate `bytes` to [`MAX_VALUE_BYTES`], on a byte boundary (this hashes
/// raw bytes, so a UTF-8 boundary is irrelevant and slicing `&[u8]` never
/// panics, unlike slicing a `&str`).
fn truncate(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.len().min(MAX_VALUE_BYTES)]
}

/// `HMAC-SHA256(secret, org_id ‖ 0 ‖ model ‖ 0 ‖ source ‖ 0 ‖ value)[..16]`.
/// `value` must already be bounded by the caller (see [`truncate`]) — each
/// candidate value is truncated at its own natural boundary (the whole
/// header/session_id/prompt_cache_key string, or each message's text
/// independently for the prefix source) rather than uniformly after
/// framing.
fn hashed_key(
    secret: &[u8; 32],
    org_id: &str,
    model: &str,
    source: &str,
    value: &[u8],
) -> AffinityKey {
    // A 32-byte key is always valid for HMAC-SHA256; this never fails.
    let mut mac = HmacSha256::new_from_slice(secret).expect("32-byte HMAC key is always valid");
    mac.update(org_id.as_bytes());
    mac.update(&[0u8]);
    mac.update(model.as_bytes());
    mac.update(&[0u8]);
    mac.update(source.as_bytes());
    mac.update(&[0u8]);
    mac.update(value);
    let digest = mac.finalize().into_bytes();

    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    AffinityKey::from_bytes(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 32] = [9u8; 32];

    fn key_hex(key: &AffinityKey) -> String {
        key.to_hex()
    }

    fn msg(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: Some(serde_json::Value::String(content.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        }
    }

    fn base_messages() -> Vec<ChatMessage> {
        vec![
            msg(MessageRole::System, "you are a helpful assistant"),
            msg(MessageRole::User, "hello there"),
        ]
    }

    #[test]
    fn header_beats_body() {
        let extra: HashMap<String, serde_json::Value> = [
            ("session_id".to_string(), serde_json::json!("body-session")),
            (
                "prompt_cache_key".to_string(),
                serde_json::json!("body-cache"),
            ),
        ]
        .into_iter()
        .collect();

        let (from_header, source) = derive(
            "org-1",
            "model-a",
            Some("header-hint"),
            &extra,
            &base_messages(),
            false,
            &SECRET,
        )
        .expect("header hint present");
        assert_eq!(source, AffinitySource::Client);

        let (from_header_only, _) = derive(
            "org-1",
            "model-a",
            Some("header-hint"),
            &HashMap::new(),
            &base_messages(),
            false,
            &SECRET,
        )
        .expect("header hint present");

        assert_eq!(
            key_hex(&from_header),
            key_hex(&from_header_only),
            "the header hint must win over body fields, so the derived key must be \
             identical whether or not the body fields are present"
        );
    }

    #[test]
    fn prompt_cache_key_used() {
        let extra: HashMap<String, serde_json::Value> = [(
            "prompt_cache_key".to_string(),
            serde_json::json!("cache-key-1"),
        )]
        .into_iter()
        .collect();

        let (from_cache_key, source) = derive(
            "org-1",
            "model-a",
            None,
            &extra,
            &base_messages(),
            false,
            &SECRET,
        )
        .expect("prompt_cache_key present");
        assert_eq!(source, AffinitySource::Client);

        // Same logical value passed as the header hint instead must produce
        // the same key (same org/model/source/value), proving prompt_cache_key
        // was actually read (and not e.g. silently ignored in favor of the
        // message-prefix fallback).
        let (from_header, _) = derive(
            "org-1",
            "model-a",
            Some("cache-key-1"),
            &HashMap::new(),
            &base_messages(),
            false,
            &SECRET,
        )
        .expect("header present");
        assert_eq!(key_hex(&from_cache_key), key_hex(&from_header));

        // And it must differ from the prefix-derived key for the same request.
        let (from_prefix, prefix_source) = derive(
            "org-1",
            "model-a",
            None,
            &HashMap::new(),
            &base_messages(),
            false,
            &SECRET,
        )
        .expect("prefix available");
        assert_eq!(prefix_source, AffinitySource::Prefix);
        assert_ne!(key_hex(&from_cache_key), key_hex(&from_prefix));
    }

    #[test]
    fn e2ee_without_client_key_has_no_affinity() {
        // E4: E2EE on, no session hint / session_id / prompt_cache_key -> no
        // affinity at all (the prefix source is unavailable under E2EE).
        let result = derive(
            "org-1",
            "model-a",
            None,
            &HashMap::new(),
            &base_messages(),
            true,
            &SECRET,
        );
        assert!(result.is_none());
    }

    #[test]
    fn same_conversation_same_key_across_turns() {
        let turn1 = base_messages();
        let (key1, source1) = derive(
            "org-1",
            "model-a",
            None,
            &HashMap::new(),
            &turn1,
            false,
            &SECRET,
        )
        .expect("prefix available");
        assert_eq!(source1, AffinitySource::Prefix);

        // Turn 2: the same first system/user messages, plus an assistant
        // reply and a follow-up user message appended.
        let mut turn2 = turn1.clone();
        turn2.push(msg(MessageRole::Assistant, "hi! how can I help?"));
        turn2.push(msg(MessageRole::User, "what's the weather like?"));

        let (key2, source2) = derive(
            "org-1",
            "model-a",
            None,
            &HashMap::new(),
            &turn2,
            false,
            &SECRET,
        )
        .expect("prefix available");
        assert_eq!(source2, AffinitySource::Prefix);

        assert_eq!(
            key_hex(&key1),
            key_hex(&key2),
            "appending later turns must not change the key derived from the first \
             system/user messages"
        );
    }

    #[test]
    fn different_orgs_different_keys() {
        let (key_a, _) = derive(
            "org-a",
            "model-a",
            Some("same-hint"),
            &HashMap::new(),
            &base_messages(),
            false,
            &SECRET,
        )
        .unwrap();
        let (key_b, _) = derive(
            "org-b",
            "model-a",
            Some("same-hint"),
            &HashMap::new(),
            &base_messages(),
            false,
            &SECRET,
        )
        .unwrap();
        assert_ne!(key_hex(&key_a), key_hex(&key_b));
    }

    #[test]
    fn huge_hint_truncated() {
        // E20: two hints that agree on the first 256 bytes but differ after
        // must derive to the same key.
        let base = "x".repeat(300);
        let mut hint_a = base.clone();
        let mut hint_b = base;
        hint_a.push_str("-tail-a");
        hint_b.push_str("-tail-b-longer");

        let (key_a, _) = derive(
            "org-1",
            "model-a",
            Some(&hint_a),
            &HashMap::new(),
            &base_messages(),
            false,
            &SECRET,
        )
        .unwrap();
        let (key_b, _) = derive(
            "org-1",
            "model-a",
            Some(&hint_b),
            &HashMap::new(),
            &base_messages(),
            false,
            &SECRET,
        )
        .unwrap();
        assert_eq!(
            key_hex(&key_a),
            key_hex(&key_b),
            "hints identical in their first 256 bytes must hash identically"
        );

        // A hint that differs within the first 256 bytes must produce a
        // different key.
        let mut hint_c = "y".repeat(300);
        hint_c.push_str("-tail-a");
        let (key_c, _) = derive(
            "org-1",
            "model-a",
            Some(&hint_c),
            &HashMap::new(),
            &base_messages(),
            false,
            &SECRET,
        )
        .unwrap();
        assert_ne!(key_hex(&key_a), key_hex(&key_c));
    }

    #[test]
    fn long_system_prompt_still_distinguishes_users() {
        // A system prompt over 256 bytes must not push the user message out
        // of the key: system and user text are each truncated to 256 bytes
        // independently, before framing. If the whole composed buffer were
        // truncated instead, two different users sharing this long system
        // prompt would collapse onto the same key (a routing hotspot).
        let long_system = "s".repeat(400);
        let messages_user_a = vec![
            msg(MessageRole::System, &long_system),
            msg(MessageRole::User, "alice's question"),
        ];
        let messages_user_b = vec![
            msg(MessageRole::System, &long_system),
            msg(MessageRole::User, "bob's totally different question"),
        ];

        let (key_a, source_a) = derive(
            "org-1",
            "model-a",
            None,
            &HashMap::new(),
            &messages_user_a,
            false,
            &SECRET,
        )
        .expect("prefix available");
        assert_eq!(source_a, AffinitySource::Prefix);

        let (key_b, _) = derive(
            "org-1",
            "model-a",
            None,
            &HashMap::new(),
            &messages_user_b,
            false,
            &SECRET,
        )
        .expect("prefix available");

        assert_ne!(
            key_hex(&key_a),
            key_hex(&key_b),
            "different users sharing a long system prompt must still get distinct keys"
        );
    }

    #[test]
    fn garbage_hint_is_fine() {
        // Arbitrary punctuation/control-ish characters must never panic and
        // must still derive a key.
        let garbage = "\u{0}\u{1}\t\n\r\"';DROP TABLE--<script>あ🦀\u{7f}";
        let result = derive(
            "org-1",
            "model-a",
            Some(garbage),
            &HashMap::new(),
            &base_messages(),
            false,
            &SECRET,
        );
        assert!(result.is_some());
    }

    #[test]
    fn no_signal_at_all_is_none() {
        let empty_messages: Vec<ChatMessage> = vec![];
        let result = derive(
            "org-1",
            "model-a",
            None,
            &HashMap::new(),
            &empty_messages,
            false,
            &SECRET,
        );
        assert!(result.is_none());
    }

    #[test]
    fn non_string_or_empty_extra_values_count_as_absent() {
        let extra: HashMap<String, serde_json::Value> = [
            ("session_id".to_string(), serde_json::json!(12345)),
            ("prompt_cache_key".to_string(), serde_json::json!("")),
        ]
        .into_iter()
        .collect();

        // Both body fields are unusable (non-string / empty), so this must
        // fall through to the prefix source rather than erroring or using
        // the non-string/empty values.
        let (_, source) = derive(
            "org-1",
            "model-a",
            None,
            &extra,
            &base_messages(),
            false,
            &SECRET,
        )
        .expect("falls back to prefix");
        assert_eq!(source, AffinitySource::Prefix);
    }
}
