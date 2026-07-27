//! Prefix-aware routing for inference cache hit optimization.
//!
//! Requests sharing the same first message (typically the system prompt) receive
//! the same deterministic routing key in every Cloud API process. The fleet
//! reduces that key modulo the current backend count, keeping the request on the
//! same indexed backend and its KV cache while the backend membership is stable.

use crate::models::{ChatMessage, MessageRole};
use sha2::{Digest, Sha256};

/// Stateless router that derives a stable key from the reusable request prefix.
#[derive(Default)]
pub struct PrefixRouter;

impl PrefixRouter {
    pub fn new() -> Self {
        Self
    }

    /// Return a deterministic routing key based on the first message.
    ///
    /// Only the first message is considered to preserve the existing affinity
    /// contract: conversations that share a system prompt remain on one backend
    /// even as later user and assistant messages change.
    pub fn route(&self, messages: &[ChatMessage]) -> u64 {
        let Some(message) = messages.first() else {
            return 0;
        };

        let mut hasher = Sha256::new();
        // Fixed-width role and content-kind tags make the outer fields
        // unambiguous; every variable-width text value is length-prefixed.
        hasher.update([role_tag(&message.role)]);

        match message.content.as_ref() {
            None => hasher.update([0]),
            Some(serde_json::Value::String(text)) => {
                hasher.update([1]);
                hash_text(&mut hasher, text);
            }
            Some(serde_json::Value::Array(parts)) => {
                hasher.update([2]);
                for part in parts {
                    if let Some(text) = part.get("text").and_then(|value| value.as_str()) {
                        hash_text(&mut hasher, text);
                    }
                }
            }
            Some(other) => {
                hasher.update([3]);
                hash_text(&mut hasher, &other.to_string());
            }
        }

        let digest = hasher.finalize();
        u64::from_be_bytes(
            digest[..8]
                .try_into()
                .expect("SHA-256 digest always contains eight bytes"),
        )
    }
}

fn role_tag(role: &MessageRole) -> u8 {
    match role {
        MessageRole::System => 0,
        MessageRole::User => 1,
        MessageRole::Assistant => 2,
        MessageRole::Tool => 3,
    }
}

fn hash_text(hasher: &mut Sha256, text: &str) {
    hasher.update((text.len() as u64).to_be_bytes());
    hasher.update(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::thread;

    fn message(role: MessageRole, content: Option<serde_json::Value>) -> ChatMessage {
        ChatMessage {
            role,
            content,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn text_message(role: MessageRole, content: &str) -> ChatMessage {
        message(role, Some(serde_json::Value::String(content.to_string())))
    }

    #[test]
    fn same_first_message_has_same_key_when_later_messages_differ() {
        let router = PrefixRouter::new();
        let messages_a = vec![
            text_message(MessageRole::System, "You are a helpful assistant."),
            text_message(MessageRole::User, "What is 2+2?"),
        ];
        let messages_b = vec![
            text_message(MessageRole::System, "You are a helpful assistant."),
            text_message(MessageRole::User, "What is the meaning of life?"),
        ];

        assert_eq!(router.route(&messages_a), router.route(&messages_b));
    }

    #[test]
    fn independent_routers_ignore_first_seen_order() {
        let router_a = PrefixRouter::new();
        let router_b = PrefixRouter::new();
        let target = vec![text_message(MessageRole::System, "shared prefix")];

        for index in 0..100 {
            let other = vec![text_message(
                MessageRole::System,
                &format!("router-a-prefix-{index}"),
            )];
            router_a.route(&other);
        }

        for index in (0..100).rev() {
            let other = vec![text_message(
                MessageRole::System,
                &format!("router-b-prefix-{index}"),
            )];
            router_b.route(&other);
        }

        assert_eq!(router_a.route(&target), router_b.route(&target));
    }

    #[test]
    fn routing_key_has_stable_test_vector() {
        let messages = vec![text_message(MessageRole::System, "shared prefix")];

        assert_eq!(
            PrefixRouter::new().route(&messages),
            15_400_765_393_054_233_284
        );
    }

    #[test]
    fn routing_is_stable_under_concurrency() {
        let router = Arc::new(PrefixRouter::new());
        let messages = Arc::new(vec![text_message(
            MessageRole::System,
            "concurrent shared prefix",
        )]);
        let expected = router.route(&messages);

        let workers: Vec<_> = (0..16)
            .map(|_| {
                let router = Arc::clone(&router);
                let messages = Arc::clone(&messages);
                thread::spawn(move || {
                    for _ in 0..100 {
                        assert_eq!(router.route(&messages), expected);
                    }
                })
            })
            .collect();

        for worker in workers {
            worker.join().expect("routing worker should not panic");
        }
    }

    #[test]
    fn role_is_part_of_the_key() {
        let router = PrefixRouter::new();
        let content = "same content";
        let keys = [
            MessageRole::System,
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
        ]
        .map(|role| router.route(&[text_message(role, content)]));

        for (index, key) in keys.iter().enumerate() {
            assert!(!keys[..index].contains(key));
        }
    }

    #[test]
    fn multipart_text_is_length_delimited_and_stable() {
        let router_a = PrefixRouter::new();
        let router_b = PrefixRouter::new();
        let multipart = vec![message(
            MessageRole::User,
            Some(json!([
                {"type": "text", "text": "ab"},
                {"type": "image_url", "image_url": {"url": "ignored"}},
                {"type": "text", "text": "c"}
            ])),
        )];
        let ambiguous_without_lengths = vec![message(
            MessageRole::User,
            Some(json!([
                {"type": "text", "text": "a"},
                {"type": "text", "text": "bc"}
            ])),
        )];

        assert_eq!(router_a.route(&multipart), router_b.route(&multipart));
        assert_ne!(
            router_a.route(&multipart),
            router_a.route(&ambiguous_without_lengths)
        );
    }

    #[test]
    fn non_text_json_content_is_stable() {
        let router_a = PrefixRouter::new();
        let router_b = PrefixRouter::new();
        let messages = vec![message(
            MessageRole::Tool,
            Some(json!({"status": "ok", "count": 3})),
        )];

        assert_eq!(router_a.route(&messages), router_b.route(&messages));
    }

    #[test]
    fn changed_first_message_changes_key() {
        let router = PrefixRouter::new();
        let first = vec![text_message(MessageRole::System, "first prefix")];
        let second = vec![text_message(MessageRole::System, "second prefix")];

        assert_ne!(router.route(&first), router.route(&second));
    }

    #[test]
    fn empty_messages_route_to_zero() {
        assert_eq!(PrefixRouter::new().route(&[]), 0);
    }

    #[test]
    fn deterministic_keys_reach_each_small_backend_index() {
        let router = PrefixRouter::new();

        for backend_count in 1_u64..=4 {
            let mut seen = vec![false; backend_count as usize];
            for index in 0..1_000 {
                let messages = vec![text_message(
                    MessageRole::System,
                    &format!("distribution-prefix-{index}"),
                )];
                seen[(router.route(&messages) % backend_count) as usize] = true;
            }
            assert!(
                seen.into_iter().all(|value| value),
                "every index should be reachable with {backend_count} backends"
            );
        }
    }
}
