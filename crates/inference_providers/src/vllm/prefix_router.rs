//! Prefix-aware routing for inference cache hit optimization.
//!
//! Maintains a trie where each level corresponds to a message in the conversation.
//! Requests sharing the same system prompt route to the same bucket regardless of
//! subsequent user messages. Each bucket maps to a persistent TLS connection pinned
//! to a specific backend via L4 passthrough.

use crate::models::ChatMessage;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::RwLock;

/// Number of prefix routing buckets (persistent connections per provider).
fn num_buckets() -> usize {
    std::env::var("NUM_PREFIX_BUCKETS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(64)
        .min(1024) // Cap to prevent excessive memory from misconfiguration
}

/// Maximum number of messages to consider for routing.
fn max_trie_depth() -> usize {
    std::env::var("PREFIX_MAX_MESSAGES")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(8)
}

struct TrieNode {
    children: HashMap<u64, Box<TrieNode>>,
    /// Bucket ID. Only root's direct children get fresh buckets (one per unique
    /// first message, typically the system prompt). All deeper nodes inherit
    /// their parent's bucket, keeping conversations with the same prefix on
    /// the same backend.
    bucket: usize,
}

/// Routes requests to buckets based on conversation prefix similarity.
///
/// Each trie level represents one message (role + content hash).
/// Requests sharing the same system prompt → same bucket → same backend → cache hit.
pub struct PrefixRouter {
    trie: RwLock<TrieNode>,
    next_bucket: AtomicUsize,
    num_buckets: usize,
    max_depth: usize,
}

impl PrefixRouter {
    pub fn new() -> Self {
        Self::with_config(num_buckets(), max_trie_depth())
    }

    fn with_config(num_buckets: usize, max_depth: usize) -> Self {
        Self {
            trie: RwLock::new(TrieNode {
                children: HashMap::new(),
                bucket: 0,
            }),
            next_bucket: AtomicUsize::new(1),
            num_buckets,
            max_depth,
        }
    }

    pub fn num_buckets(&self) -> usize {
        self.num_buckets
    }

    /// Route a request to a bucket based on its conversation prefix.
    pub fn route(&self, messages: &[ChatMessage]) -> usize {
        let hashes = hash_messages(messages, self.max_depth);
        if hashes.is_empty() {
            return 0;
        }

        // Fast path: read-only lookup. Since deeper nodes inherit parent buckets,
        // this always returns a valid bucket even for partially-unseen prefixes.
        // Only take write lock when the first message (system prompt) is brand new.
        let trie = self.trie.read().unwrap_or_else(|e| e.into_inner());
        let first_msg_exists = trie.children.contains_key(&hashes[0]);
        let bucket = Self::lookup_readonly(&trie, &hashes);
        drop(trie);

        if first_msg_exists {
            return bucket;
        }

        // Slow path: first message unseen — need to assign a new bucket
        let mut trie = self.trie.write().unwrap_or_else(|e| e.into_inner());
        self.lookup_or_insert(&mut trie, &hashes)
    }

    /// Read-only lookup. Returns the deepest matching node's bucket.
    /// Since deeper nodes inherit the parent's bucket, we can return early
    /// when we hit an unseen message — the parent's bucket is correct.
    fn lookup_readonly(node: &TrieNode, hashes: &[u64]) -> usize {
        let mut current = node;
        for h in hashes {
            match current.children.get(h) {
                Some(child) => current = child,
                None => break, // Unseen message — parent's bucket is inherited
            }
        }
        current.bucket
    }

    /// Only root's direct children get fresh buckets. All deeper nodes inherit
    /// their parent's bucket.
    fn lookup_or_insert(&self, root: &mut TrieNode, hashes: &[u64]) -> usize {
        let mut node = root;
        let mut is_root = true;

        for h in hashes {
            let parent_bucket = node.bucket;

            if !node.children.contains_key(h) {
                let bucket = if is_root {
                    self.next_bucket.fetch_add(1, Ordering::Relaxed) % self.num_buckets
                } else {
                    parent_bucket
                };
                node.children.insert(
                    *h,
                    Box::new(TrieNode {
                        children: HashMap::new(),
                        bucket,
                    }),
                );
            }

            node = node.children.get_mut(h).unwrap();
            is_root = false;
        }
        node.bucket
    }
}

/// Hash each message to produce trie edge keys.
fn hash_messages(messages: &[ChatMessage], max_depth: usize) -> Vec<u64> {
    let limit = messages.len().min(max_depth);
    let mut hashes = Vec::with_capacity(limit);

    for msg in &messages[..limit] {
        let mut hasher = DefaultHasher::new();
        let role_tag: u8 = match msg.role {
            crate::models::MessageRole::System => 0,
            crate::models::MessageRole::User => 1,
            crate::models::MessageRole::Assistant => 2,
            crate::models::MessageRole::Tool => 3,
        };
        role_tag.hash(&mut hasher);
        if let Some(ref content) = msg.content {
            match content {
                serde_json::Value::String(s) => s.hash(&mut hasher),
                serde_json::Value::Array(parts) => {
                    for part in parts {
                        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                            text.hash(&mut hasher);
                        }
                    }
                }
                other => other.to_string().hash(&mut hasher),
            }
        }
        hashes.push(hasher.finish());
    }
    hashes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MessageRole;

    fn msg(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: Some(serde_json::Value::String(content.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn router() -> PrefixRouter {
        PrefixRouter::with_config(64, 8)
    }

    #[test]
    fn test_same_system_prompt_same_bucket() {
        let router = router();
        let msgs1 = vec![
            msg(MessageRole::System, "You are a helpful assistant."),
            msg(MessageRole::User, "What is 2+2?"),
        ];
        let msgs2 = vec![
            msg(MessageRole::System, "You are a helpful assistant."),
            msg(MessageRole::User, "What is the meaning of life?"),
        ];
        let b1 = router.route(&msgs1);
        let b2 = router.route(&msgs2);
        assert_eq!(b1, b2, "Same system prompt → same bucket");
    }

    #[test]
    fn test_identical_conversations_same_bucket() {
        let router = router();
        let msgs = vec![
            msg(MessageRole::System, "You are helpful."),
            msg(MessageRole::User, "Hi"),
        ];
        assert_eq!(router.route(&msgs), router.route(&msgs));
    }

    #[test]
    fn test_different_system_prompts_different_buckets() {
        let router = router();
        let msgs1 = vec![msg(MessageRole::System, "You are a Python expert.")];
        let msgs2 = vec![msg(MessageRole::System, "You are a Rust expert.")];
        assert_ne!(router.route(&msgs1), router.route(&msgs2));
    }

    #[test]
    fn test_empty_messages() {
        let router = router();
        assert_eq!(router.route(&[]), 0);
    }

    #[test]
    fn test_bucket_range() {
        let router = router();
        for i in 0..100 {
            let msgs = vec![msg(MessageRole::User, &format!("message {i}"))];
            assert!(router.route(&msgs) < router.num_buckets());
        }
    }
}
