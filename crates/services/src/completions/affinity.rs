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

/// Concatenate the `type: "text"` parts of a message's content, in order,
/// stopping once [`MAX_VALUE_BYTES`] bytes have been collected. A plain
/// string content is sliced to the same bound directly. Image/audio parts
/// (and any other non-text part) are skipped. Returns an empty buffer for
/// absent content.
///
/// Bounding extraction here — instead of concatenating the full (possibly
/// huge) message text and truncating afterward — avoids an allocation
/// proportional to prompt size. The result is byte-identical to truncating
/// the old full concatenation to `MAX_VALUE_BYTES`: truncation is
/// byte-based (see [`truncate`]) and part order is preserved, so stopping
/// early once the bound is hit yields the same leading bytes.
fn message_text(content: &Option<serde_json::Value>) -> Vec<u8> {
    match content {
        Some(serde_json::Value::String(s)) => truncate(s.as_bytes()).to_vec(),
        Some(serde_json::Value::Array(parts)) => {
            let mut out = Vec::new();
            for part in parts {
                if out.len() >= MAX_VALUE_BYTES {
                    break;
                }
                if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                        let remaining = MAX_VALUE_BYTES - out.len();
                        let bytes = text.as_bytes();
                        out.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
                    }
                }
            }
            out
        }
        _ => Vec::new(),
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
        (system_role, system_text.as_slice()),
        (user_role, user_text.as_slice()),
    ] {
        write_length_prefixed(&mut buf, role.as_bytes());
        // `text` is already bounded to `MAX_VALUE_BYTES` by `message_text`.
        write_length_prefixed(&mut buf, text);
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

/// HKDF `info` for the affinity-key HMAC secret (see [`derive`]).
const AFFINITY_SECRET_INFO: &[u8] = b"nearai-placement-affinity-v1";
/// HKDF `info` for the follow-pin HMAC secret (`placement::decision::Placer`).
const PIN_SECRET_INFO: &[u8] = b"nearai-placement-pin-v1";

/// Derives `(affinity_secret, pin_secret)` from the placement Valkey
/// password with HKDF-SHA256: the password is the input key material, there
/// is no salt (HKDF then uses a zero-filled salt; the password is already a
/// high-entropy secret, and every cloud-api node must derive the same keys),
/// and the two `info` strings domain-separate the outputs. Deterministic, so
/// all nodes agree on affinity keys and pin ids without sharing more state.
/// Never log the password or either output.
///
/// # Rotation
/// Changing `PLACEMENT_REDIS_PASSWORD` invalidates every derived affinity
/// key and pin id cluster-wide: it is the sole input key material, so a new
/// password produces entirely different HMAC secrets. Nodes restart at
/// different times during a rollout, so they briefly disagree on the
/// current secret and fall back to legacy routing until every node has
/// picked up the new password. Treat rotation as a coordinated restart, not
/// a routine config change. Because the password is the sole input key
/// material, it must be high-entropy — its entropy is inherited directly by
/// the derived HMAC secrets.
///
/// Cost of a rotation: the affinity and pin keys are derived from
/// `PLACEMENT_REDIS_PASSWORD`, so rotating it re-homes every conversation
/// once. Each one's next turn lands on a new HRW home with a cold prefix
/// cache (one cold prefill per conversation), and old pins are never read
/// again. A separate HMAC secret, rotated independently of the Valkey
/// password, is future work.
pub fn secrets_from(password: &str) -> ([u8; 32], [u8; 32]) {
    let hk = hkdf::Hkdf::<Sha256>::new(None, password.as_bytes());
    let mut affinity = [0u8; 32];
    let mut pin = [0u8; 32];
    hk.expand(AFFINITY_SECRET_INFO, &mut affinity)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    hk.expand(PIN_SECRET_INFO, &mut pin)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    (affinity, pin)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 32] = [9u8; 32];

    /// A stable fingerprint for comparing keys (`AffinityKey` deliberately
    /// has no `PartialEq`/`Debug`): the key's base-tier pin id.
    fn key_hex(key: &AffinityKey) -> String {
        placement::affinity::pin_id(placement::policy::Tier::Base, key, &[0u8; 32]).to_hex()
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
    fn secrets_are_deterministic_and_distinct() {
        let (affinity, pin) = secrets_from("router-password");
        assert_eq!(secrets_from("router-password"), (affinity, pin));
        assert_ne!(
            affinity, pin,
            "affinity and pin secrets are domain-separated"
        );
        let (other_affinity, other_pin) = secrets_from("another-password");
        assert_ne!(affinity, other_affinity);
        assert_ne!(pin, other_pin);
        assert_ne!(affinity, [0u8; 32]);
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
    fn huge_multipart_message_hashes_the_same_as_its_first_256_bytes() {
        // `message_text` stops accumulating once MAX_VALUE_BYTES is hit
        // instead of concatenating the whole (potentially huge) message
        // first. The derived key must still be byte-identical to hashing
        // exactly the first 256 bytes of the concatenated text parts — what
        // "concatenate everything, then truncate" would have produced.
        let full_text: String = "abcdefghij".repeat(10_000); // 100,000 bytes
        let parts = serde_json::json!([
            {"type": "text", "text": full_text},
            {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}},
            {"type": "text", "text": "more text that must never be reached"},
        ]);
        let huge_msg = ChatMessage {
            role: MessageRole::User,
            content: Some(parts),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        };
        let messages_huge = vec![msg(MessageRole::System, "sys"), huge_msg];

        let truncated_text = &full_text[..MAX_VALUE_BYTES];
        let messages_truncated = vec![
            msg(MessageRole::System, "sys"),
            msg(MessageRole::User, truncated_text),
        ];

        let (key_huge, _) = derive(
            "org-1",
            "model-a",
            None,
            &HashMap::new(),
            &messages_huge,
            false,
            &SECRET,
        )
        .expect("prefix available");
        let (key_truncated, _) = derive(
            "org-1",
            "model-a",
            None,
            &HashMap::new(),
            &messages_truncated,
            false,
            &SECRET,
        )
        .expect("prefix available");

        assert_eq!(
            key_hex(&key_huge),
            key_hex(&key_truncated),
            "a huge multi-part message must hash the same as its first 256 bytes"
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
