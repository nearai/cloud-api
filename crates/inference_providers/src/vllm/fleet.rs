//! `FleetRouter` — the per-provider routing state for NEAR-AI model-proxy
//! backends: the prefix-bucket and rotation-SNI mappings used to send a
//! completion and its later signature fetch to the *same* backend through
//! model-proxy's per-TCP L4 load balancer.
//!
//! Extracted from `VLlmProvider` so this routing state lives in one place.
//! Today it keys on the model-proxy rotation *index* (`u64`); P3 swaps that for
//! a stable `BackendHandle` here, without touching the completion path.
//!
//! This is a mechanical move of existing behavior — the methods below are the
//! verbatim logic previously inlined on `VLlmProvider`, guarded by the
//! characterization tests in the parent module.

use super::prefix_router::PrefixRouter;
use crate::rotation;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Poison-tolerant lock: a panicked holder shouldn't wedge routing — we only
/// ever mutate small maps under it, so recovering the inner value is safe.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) struct FleetRouter {
    /// request_hash → bucket_id during streaming (before the chat_id is known).
    pub(super) pending_buckets: Mutex<HashMap<String, usize>>,
    /// chat_id → bucket_id, so the signature fetch reuses the bucket's pinned
    /// H2 connection to the backend that served the completion.
    pub(super) signature_buckets: Mutex<HashMap<String, usize>>,
    /// request_hash → rotation index when a streaming attempt fell over to a
    /// rotation-SNI backend (before the chat_id is known).
    pub(super) pending_rotation: Mutex<HashMap<String, u64>>,
    /// chat_id → rotation index for the signature fetch path.
    pub(super) signature_rotation: Mutex<HashMap<String, u64>>,
    /// Most recent healthy backend count reported by discovery; bounds the
    /// rotation-SNI fan-out. Read with `Relaxed` (best-effort).
    pub(super) last_backend_count: AtomicUsize,
    /// Pre-parsed rotation parts from the provider's base_url. `None` for URLs
    /// that don't fit the rotation scheme (one-label host, IP literal, …) — then
    /// rotation is a no-op and the canonical-SNI error propagates as before.
    rotation_parts: Option<rotation::UrlParts>,
    /// Message-prefix trie mapping a conversation prefix to a bucket id, so
    /// requests sharing a prefix stick to the same backend (prefix-cache hit).
    pub(super) prefix_router: Arc<PrefixRouter>,
    /// Lazily-filled (or eagerly pre-created in legacy mode) per-bucket clients,
    /// each pinning a persistent H2 connection to one verified backend. The
    /// provider fills/clears these slots via inline attestation; FleetRouter
    /// just owns the storage.
    pub(super) bucket_clients: Vec<Mutex<Option<Client>>>,
}

impl FleetRouter {
    pub(super) fn new(
        rotation_parts: Option<rotation::UrlParts>,
        prefix_router: Arc<PrefixRouter>,
        bucket_clients: Vec<Mutex<Option<Client>>>,
    ) -> Self {
        Self {
            pending_buckets: Mutex::new(HashMap::new()),
            signature_buckets: Mutex::new(HashMap::new()),
            pending_rotation: Mutex::new(HashMap::new()),
            signature_rotation: Mutex::new(HashMap::new()),
            last_backend_count: AtomicUsize::new(0),
            rotation_parts,
            prefix_router,
            bucket_clients,
        }
    }

    /// Route a request's messages to a prefix bucket id.
    pub(super) fn route(&self, messages: &[crate::ChatMessage]) -> usize {
        self.prefix_router.route(messages)
    }

    /// Number of rotation-SNI indices to fan out across, clamped to the
    /// fan-out cap. `0` when rotation is disabled (no rotation parts) or
    /// discovery hasn't reported a backend count yet — the signal to skip the
    /// rotation path and propagate the canonical error.
    pub(super) fn rotation_count(&self) -> usize {
        if self.rotation_parts.is_none() {
            return 0;
        }
        self.backend_count().min(rotation::MAX_FANOUT)
    }

    /// Build the absolute URL `https://<canonical>-i<index>.<base><path>` for a
    /// rotation attempt at the given backend index. `None` only when rotation
    /// parts are missing — callers should have filtered via `rotation_count()`.
    pub(super) fn rotation_url(&self, index: u64, path: &str) -> Option<String> {
        let parts = self.rotation_parts.as_ref()?;
        let mut url = rotation::rotation_base_url(parts, index)?;
        url.set_path(path);
        Some(url.to_string())
    }

    /// Promote the pre-chat_id mappings (keyed by request_hash) onto the
    /// chat_id, so `get_signature` reuses the same bucket/rotation backend.
    /// Empty chat_id (orphan-cleanup) drains the pending rotation entry without
    /// writing `signature_rotation`.
    pub(super) fn pin_chat_connection(&self, request_hash: &str, chat_id: &str) {
        if let Some(bucket_id) = lock(&self.pending_buckets).remove(request_hash) {
            lock(&self.signature_buckets).insert(chat_id.to_string(), bucket_id);
        }
        if let Some(index) = lock(&self.pending_rotation).remove(request_hash) {
            if !chat_id.is_empty() {
                lock(&self.signature_rotation).insert(chat_id.to_string(), index);
            }
        }
    }

    pub(super) fn unpin_chat_connection(&self, chat_id: &str) {
        lock(&self.signature_buckets).remove(chat_id);
        lock(&self.signature_rotation).remove(chat_id);
    }

    pub(super) fn set_backend_count(&self, count: usize) {
        self.last_backend_count.store(count, Ordering::Relaxed);
    }

    /// Latest healthy backend count (best-effort, `Relaxed`).
    pub(super) fn backend_count(&self) -> usize {
        self.last_backend_count.load(Ordering::Relaxed)
    }
}
