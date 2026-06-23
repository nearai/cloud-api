//! `Fleet` — the per-provider routing state for NEAR-AI model-proxy
//! backends: the prefix-affinity and rotation-index mappings used to send a
//! completion and its later signature fetch to the *same* backend through
//! model-proxy's per-TCP L4 load balancer.
//!
//! Extracted from `Provider` so this routing state lives in one place.
//!
//! Backend addressing is index-addressed: model-proxy publishes a synthetic SNI
//! `<canonical>-i<N>.<base>` that routes a fresh TCP to backend `N %
//! healthy_count` deterministically (`rotation.rs`). Slot `i` of `index_clients`
//! is a pooled, attestation-verified H2 client pinned to backend `i`. We keep a
//! per-index TTFT EMA so we can steer prefix-affinity routing away from a
//! pathologically slow backend.

use super::prefix_router::PrefixRouter;
use super::Config;
use crate::rotation;
use crate::spki_verifier::FingerprintState;
use crate::BackendVerifier;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use tokio::sync::Semaphore;

/// EMA smoothing for per-backend TTFT: fast warmup then stable.
const TTFT_EWMA_ALPHA_WARMUP: f64 = 0.5;
const TTFT_EWMA_ALPHA_STABLE: f64 = 0.1;
// `pub(super)` so the parent module's tests can drive a backend past warmup.
pub(super) const TTFT_WARMUP_SAMPLES: u32 = 8;
/// A backend is "slow" (steer away) when its EMA exceeds this multiple of the
/// fastest peer's EMA AND the absolute floor below.
const TTFT_SLOW_RATIO: f64 = 2.0;
const TTFT_SLOW_FLOOR_MS: f64 = 500.0;

#[derive(Default, Clone, Copy)]
pub(super) struct BackendStat {
    pub(super) ttft_ewma_ms: f64,
    pub(super) samples: u32,
}

/// Fold a freshly observed TTFT sample into a backend's EMA. Shared by the
/// `Fleet::record_ttft` method and the provider-internal `TtftProbe` stream
/// wrapper (which only holds a clone of the `backend_stats` Arc, not `&Fleet`).
pub(super) fn update_ema(stat: &mut BackendStat, ttft_ms: f64) {
    if ttft_ms <= 0.0 {
        return;
    }
    let alpha = if stat.samples < TTFT_WARMUP_SAMPLES {
        TTFT_EWMA_ALPHA_WARMUP
    } else {
        TTFT_EWMA_ALPHA_STABLE
    };
    stat.ttft_ewma_ms = if stat.ttft_ewma_ms == 0.0 {
        ttft_ms
    } else {
        alpha * ttft_ms + (1.0 - alpha) * stat.ttft_ewma_ms
    };
    stat.samples = stat.samples.saturating_add(1);
}

/// Poison-tolerant lock: a panicked holder shouldn't wedge routing — we only
/// ever mutate small maps under it, so recovering the inner value is safe.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) struct Fleet {
    /// request_hash → rotation index during streaming (before the chat_id is
    /// known). Universal completion→signature index map for the streaming path.
    pub(super) pending_rotation: Mutex<HashMap<String, u64>>,
    /// chat_id → rotation index for the signature fetch path, so the signature
    /// is fetched from the same backend that served the completion.
    pub(super) signature_rotation: Mutex<HashMap<String, u64>>,
    /// Most recent healthy backend count reported by discovery; bounds the
    /// rotation-SNI fan-out. Read with `Relaxed` (best-effort).
    pub(super) last_backend_count: AtomicUsize,
    /// Pre-parsed rotation parts from the provider's base_url. `None` for URLs
    /// that don't fit the rotation scheme (one-label host, IP literal, …) — then
    /// rotation is a no-op and the canonical-SNI path is used.
    rotation_parts: Option<rotation::UrlParts>,
    /// Message-prefix trie mapping a conversation prefix to a bucket id, so
    /// requests sharing a prefix stick to the same backend (prefix-cache hit).
    /// The bucket id is reduced modulo the live backend count to a rotation
    /// index by `select_index`.
    pub(super) prefix_router: Arc<PrefixRouter>,
    /// Lazily-filled (or eagerly pre-created in legacy mode) per-backend-index
    /// clients, each pinning a persistent H2 connection to one verified
    /// backend. Slot `i` pins `<canonical>-i<i>.<base>` (backend i). Sized to
    /// `rotation::MAX_FANOUT`. The provider fills/clears these slots via inline
    /// attestation; Fleet just owns the storage.
    pub(super) index_clients: Vec<Mutex<Option<Client>>>,
    /// Per-backend-index TTFT EMA for latency-aware steering. Index == rotation
    /// index. Arc so the stream-measurement wrapper can update it after the
    /// Fleet method returns. Sized to MAX_FANOUT.
    pub(super) backend_stats: Arc<Mutex<Vec<BackendStat>>>,
    /// Provider config (base_url, api_key, timeouts).
    pub(super) config: Config,
    /// General-purpose client for non-completion requests (attestation, models).
    pub(super) client: Client,
    /// Completion-timeout, non-pinned client used for the canonical fallback
    /// (cold-start / non-rotation) and when inline index verification exhausts
    /// retries (graceful degradation).
    pub(super) fallback_client: Client,
    /// Bounds concurrent inline verifications (thundering-herd guard).
    pub(super) verification_semaphore: Arc<Semaphore>,
    /// TLS fingerprint pin state shared by the general client + all index and
    /// rotation clients.
    pub(super) fingerprint_state: Arc<RwLock<FingerprintState>>,
    /// Builds verified clients for lazy index init (None in legacy/test mode).
    pub(super) backend_verifier: Option<Arc<dyn BackendVerifier>>,
}

impl Fleet {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        rotation_parts: Option<rotation::UrlParts>,
        prefix_router: Arc<PrefixRouter>,
        index_clients: Vec<Mutex<Option<Client>>>,
        config: Config,
        client: Client,
        fallback_client: Client,
        verification_semaphore: Arc<Semaphore>,
        fingerprint_state: Arc<RwLock<FingerprintState>>,
        backend_verifier: Option<Arc<dyn BackendVerifier>>,
    ) -> Self {
        Self {
            pending_rotation: Mutex::new(HashMap::new()),
            signature_rotation: Mutex::new(HashMap::new()),
            last_backend_count: AtomicUsize::new(0),
            rotation_parts,
            prefix_router,
            index_clients,
            backend_stats: Arc::new(Mutex::new(vec![
                BackendStat::default();
                rotation::MAX_FANOUT
            ])),
            config,
            client,
            fallback_client,
            verification_semaphore,
            fingerprint_state,
            backend_verifier,
        }
    }

    /// Select the backend rotation index for a request: prefix-affinity
    /// (same prefix → same backend → KV-cache hit) with preemptive latency
    /// steering away from a backend whose TTFT EMA is pathological. Returns
    /// `None` when rotation is unavailable (count==0 / no rotation parts) →
    /// caller uses the canonical fallback path.
    ///
    /// Index↔backend stability: `<canonical>-iN` routes to backend `N % count`
    /// at model-proxy by SNI (independent of which TCP connection we use), so a
    /// pinned index is stable — and the completion→signature pin holds — only
    /// while the healthy count AND backend membership are unchanged. A count
    /// change resets clients + EMA (see `store_backend_count`); a same-count
    /// membership change (one backend drops as another recovers) can silently
    /// remap index `i`, so an in-flight signature pin can briefly resolve to the
    /// wrong backend and 404. That window is small (signatures are fetched
    /// within the caller's FINALIZE_TIMEOUT, ~seconds; topology changes are
    /// ~5-min discovery cadence) and degrades gracefully (the missing signature
    /// is logged, the completion still streams). This matches the pre-existing
    /// rotation-fallback behavior; it is not introduced by index-addressing.
    ///
    /// Reachability: `prefix_router.route()` returns a bucket id in
    /// `0..NUM_PREFIX_BUCKETS` (default 64), reduced mod `count`. When `count >
    /// NUM_PREFIX_BUCKETS` (more than 64 live backends for one model — far above
    /// any current deployment) the high indices are unreachable as the
    /// *preferred* pick, though latency steering / fallback can still reach
    /// them. Raise `NUM_PREFIX_BUCKETS` if a model ever exceeds 64 backends.
    pub(super) fn select_index(&self, messages: &[crate::ChatMessage]) -> Option<usize> {
        let count = self.rotation_count();
        if count == 0 {
            return None;
        }
        let preferred = self.prefix_router.route(messages) % count;
        let stats = lock(&self.backend_stats);
        let warmed =
            |i: usize| stats[i].samples >= TTFT_WARMUP_SAMPLES && stats[i].ttft_ewma_ms > 0.0;
        let min_warm = (0..count)
            .filter(|&i| warmed(i))
            .map(|i| stats[i].ttft_ewma_ms)
            .fold(f64::MAX, f64::min);
        if warmed(preferred)
            && stats[preferred].ttft_ewma_ms > TTFT_SLOW_FLOOR_MS
            && min_warm.is_finite()
            && stats[preferred].ttft_ewma_ms > TTFT_SLOW_RATIO * min_warm
        {
            // Steer to the fastest warmed backend; ties/unwarmed keep preferred.
            let mut best = preferred;
            let mut best_ms = stats[preferred].ttft_ewma_ms;
            for i in 0..count {
                if warmed(i) && stats[i].ttft_ewma_ms < best_ms {
                    best = i;
                    best_ms = stats[i].ttft_ewma_ms;
                }
            }
            return Some(best);
        }
        Some(preferred)
    }

    /// Record an observed TTFT (ms) for a backend index into its EMA.
    ///
    /// The streaming hot path measures TTFT lazily via the `TtftProbe` stream
    /// wrapper (which updates the EMA through `update_ema` without `&Fleet`), so
    /// this synchronous helper is currently used only by the unit tests that
    /// seed per-index latencies; it stays as the canonical record entry point.
    #[allow(dead_code)]
    pub(super) fn record_ttft(&self, index: usize, ttft_ms: f64) {
        if ttft_ms <= 0.0 {
            return;
        }
        let mut stats = lock(&self.backend_stats);
        let Some(s) = stats.get_mut(index) else {
            return;
        };
        update_ema(s, ttft_ms);
    }

    /// Ordering of indices to try as fallback after `tried` returned 5xx,
    /// fastest-EMA first (unwarmed backends sorted last, stable by index).
    pub(super) fn fallback_indices(&self, tried: usize) -> Vec<usize> {
        let count = self.rotation_count();
        let stats = lock(&self.backend_stats);
        let mut idxs: Vec<usize> = (0..count).filter(|&i| i != tried).collect();
        idxs.sort_by(|&a, &b| {
            let key = |i: usize| {
                let s = stats[i];
                if s.samples >= TTFT_WARMUP_SAMPLES && s.ttft_ewma_ms > 0.0 {
                    (0u8, s.ttft_ewma_ms)
                } else {
                    (1u8, 0.0)
                }
            };
            key(a)
                .partial_cmp(&key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        idxs
    }

    /// Number of rotation-SNI indices to fan out across, clamped to the
    /// fan-out cap. `0` when rotation is disabled (no rotation parts) or
    /// discovery hasn't reported a backend count yet — the signal to use the
    /// canonical fallback path.
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

    /// Promote the pre-chat_id mapping (keyed by request_hash) onto the
    /// chat_id, so `get_signature` reuses the same backend index. Empty chat_id
    /// (orphan-cleanup) drains the pending rotation entry without writing
    /// `signature_rotation`.
    // NB: these inherent helpers are deliberately named differently from the
    // `InferenceProvider` trait methods (pin_chat_connection / ...). The trait
    // impl forwards to these; distinct names make that delegation unambiguous
    // and rule out the accidental-self-recursion footgun that a same-named
    // inherent/trait pair invites (cf. the get_attestation_report fix).
    pub(super) fn pin_chat(&self, request_hash: &str, chat_id: &str) {
        if let Some(index) = lock(&self.pending_rotation).remove(request_hash) {
            if !chat_id.is_empty() {
                lock(&self.signature_rotation).insert(chat_id.to_string(), index);
            }
        }
    }

    pub(super) fn unpin_chat(&self, chat_id: &str) {
        lock(&self.signature_rotation).remove(chat_id);
    }

    pub(super) fn store_backend_count(&self, count: usize) {
        let prev = self.last_backend_count.swap(count, Ordering::Relaxed);
        if prev != count {
            // index↔backend binding via `-iN` is only stable while the healthy
            // count is stable; drop pinned clients + EMA so we re-verify and
            // re-measure against the new mapping.
            for slot in &self.index_clients {
                *lock(slot) = None;
            }
            let mut stats = lock(&self.backend_stats);
            for s in stats.iter_mut() {
                *s = BackendStat::default();
            }
        }
    }

    /// Latest healthy backend count (best-effort, `Relaxed`).
    pub(super) fn backend_count(&self) -> usize {
        self.last_backend_count.load(Ordering::Relaxed)
    }
}
