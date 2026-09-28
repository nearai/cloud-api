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

use super::placement_report::{prompt_tokens_est, report_decision, PlacementRequest};
use super::prefix_router::PrefixRouter;
use super::Config;
use crate::placement_io::{PlacementHandles, Write};
use crate::rotation;
use crate::spki_verifier::FingerprintState;
use crate::BackendHosts;
use crate::BackendVerifier;
use arc_swap::{ArcSwap, ArcSwapOption};
use placement::consts::COVERED_MODELS;
use placement::decision::{Decision, DecisionRecord, LegacyReason, PlaceInput};
use placement::score::Pending;
use placement::snapshot::Snapshot;
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
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
/// Keep a small first-turn burst for a reusable prefix on its deterministic
/// primary before spilling overlapping requests to another backend. This
/// preserves a single cache copy for ordinary traffic while preventing one hot
/// prefix from monopolizing a replica. The counter is process-local and tracks
/// only live requests; it never stores message content.
const PREFIX_AFFINITY_BURST: u32 = 4;
/// Minimum interval between warnings for a stale or unknown pinned key.
const UNKNOWN_KEY_WARN_INTERVAL_MS: u64 = 60_000;

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
    stat.ttft_ewma_ms = if stat.samples == 0 {
        ttft_ms
    } else {
        alpha * ttft_ms + (1.0 - alpha) * stat.ttft_ewma_ms
    };
    stat.samples = stat.samples.saturating_add(1);
}

/// Poison-tolerant lock: a panicked holder shouldn't wedge routing — we only
/// ever mutate small maps under it, so recovering the inner value is safe.
/// Placed (req, tok) per host, keyed by unix second.
type PlacementLedger = HashMap<u64, HashMap<String, Pending>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

type PrefixLoads = HashMap<u64, Vec<u32>>;

/// Outcome of resolving a pinned pubkey against the discovery key map.
pub(super) enum KeyGroup {
    /// No map yet, or a homogeneous fleet: route unrestricted, silently.
    Unrestricted,
    /// The map is populated but does not know this key: route unrestricted
    /// and warn because the client may be holding a stale attestation.
    UnknownKey,
    /// Route only to these indices.
    Indices(Vec<usize>),
}

impl KeyGroup {
    fn indices(&self) -> Option<&[usize]> {
        match self {
            Self::Unrestricted | Self::UnknownKey => None,
            Self::Indices(indices) => Some(indices),
        }
    }
}

/// Reservation for one live affinity-routed request. Releasing is RAII-based
/// so cancellation, errors, and dropped streams cannot leak routing load.
pub(super) struct RouteLease {
    route_key: u64,
    index: usize,
    prefix_loads: Arc<Mutex<PrefixLoads>>,
}

impl RouteLease {
    pub(super) fn index(&self) -> usize {
        self.index
    }

    pub(super) fn route_key(&self) -> u64 {
        self.route_key
    }
}

impl Drop for RouteLease {
    fn drop(&mut self) {
        let mut loads = lock(&self.prefix_loads);
        let remove = if let Some(counts) = loads.get_mut(&self.route_key) {
            if let Some(count) = counts.get_mut(self.index) {
                *count = count.saturating_sub(1);
            }
            counts.iter().all(|count| *count == 0)
        } else {
            false
        };
        if remove {
            loads.remove(&self.route_key);
        }
    }
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
    /// Stateless prefix/conversation router. Stable keys are reduced modulo the
    /// live backend count, so every Cloud API process agrees on first-turn
    /// prefix primaries and established-conversation homes.
    pub(super) prefix_router: Arc<PrefixRouter>,
    /// Lazily-filled (or eagerly pre-created in legacy mode) per-backend-index
    /// clients, each pinning a persistent H2 connection to one verified
    /// backend. Slot `i` pins `<canonical>-i<i>.<base>` (backend i). Sized to
    /// `rotation::MAX_FANOUT`. The provider fills/clears these slots via inline
    /// attestation; Fleet just owns the storage.
    pub(super) index_clients: Vec<Mutex<Option<Client>>>,
    /// Per-index time of the last inline verification that failed the TLS
    /// channel-binding check. Such a failure repeats for the same backend, so
    /// the index is not verified again until the backoff has passed (see
    /// `get_or_verify_index_client`). Same length as `index_clients`.
    pub(super) channel_binding_failed_at: Vec<Mutex<Option<tokio::time::Instant>>>,
    /// Per-backend-index TTFT EMA for latency-aware steering. Index == rotation
    /// index. Arc so the stream-measurement wrapper can update it after the
    /// Fleet method returns. Sized to MAX_FANOUT.
    pub(super) backend_stats: Arc<Mutex<Vec<BackendStat>>>,
    /// Live request counts by routing key and backend index. Entries exist only
    /// while a request is in flight and contain no prompt or response content.
    prefix_loads: Arc<Mutex<PrefixLoads>>,
    /// Pubkey (lowercase hex) to rotation indices that can serve it, from the
    /// last discovery cycle. Empty means no restriction. A backend-count
    /// change clears this because the index-to-backend binding is then stale.
    backend_keys: Arc<RwLock<HashMap<String, Vec<usize>>>>,
    /// Verified host map from the last discovery cycle (host_id -> backend
    /// index, plus attested keys); empty until the first cycle. Rebuilt
    /// wholesale each cycle. The placement hook maps a placed host to its
    /// index through this map. Every update is mirrored into the installed
    /// `PlacementHandles::hosts`, the map the Valkey reader verifies frames
    /// against.
    backend_hosts: Arc<ArcSwap<BackendHosts>>,
    /// Smart-placement handles; `None` (every request on the legacy path)
    /// until `set_placement`. Lock-free on the hot path.
    placement: ArcSwapOption<PlacementHandles>,
    /// This node's own placed (req, tok) per host, bucketed by unix second.
    /// Only the current and previous second are kept, the same window the
    /// Valkey routed hash covers. Held from reading it into `place()` to
    /// reserving the chosen host (see `try_place`), never across an await.
    placement_ledger: Mutex<PlacementLedger>,
    /// Epoch-ms of the last `UnknownKey` warning, so a client stuck on a stale
    /// attestation cannot flood the log from the request hot path.
    last_unknown_key_warn_ms: AtomicU64,
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
        let channel_binding_failed_at = index_clients.iter().map(|_| Mutex::new(None)).collect();
        Self {
            pending_rotation: Mutex::new(HashMap::new()),
            signature_rotation: Mutex::new(HashMap::new()),
            last_backend_count: AtomicUsize::new(0),
            rotation_parts,
            prefix_router,
            index_clients,
            channel_binding_failed_at,
            backend_stats: Arc::new(Mutex::new(vec![
                BackendStat::default();
                rotation::MAX_FANOUT
            ])),
            prefix_loads: Arc::new(Mutex::new(HashMap::new())),
            backend_keys: Arc::new(RwLock::new(HashMap::new())),
            backend_hosts: Arc::new(ArcSwap::from_pointee(BackendHosts::default())),
            placement: ArcSwapOption::empty(),
            placement_ledger: Mutex::new(HashMap::new()),
            last_unknown_key_warn_ms: AtomicU64::new(0),
            config,
            client,
            fallback_client,
            verification_semaphore,
            fingerprint_state,
            backend_verifier,
        }
    }

    /// Select the idle preferred backend for a request: deterministic
    /// prefix-affinity with preemptive latency steering away from a backend
    /// whose TTFT EMA is pathological. The live request path uses
    /// `acquire_index`, which starts here and adds bounded hot-prefix spillover.
    /// Returns `None` when rotation is unavailable (count==0 / no rotation
    /// parts), so the caller uses the canonical fallback path.
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
    /// Reachability: the prefix or conversation router returns a deterministic
    /// `u64`. Reducing it modulo `count` makes every live backend index
    /// reachable as the stateless primary. Process-local state affects only
    /// temporary first-turn spillover while requests overlap.
    #[cfg(test)]
    pub(super) fn select_index(
        &self,
        messages: &[crate::ChatMessage],
        pinned_pub_key: Option<&str>,
    ) -> Option<usize> {
        let count = self.rotation_count();
        if count == 0 {
            return None;
        }
        let key_group = self.resolve_key_group(pinned_pub_key, count);
        let route_key = self.route_key(messages);
        self.candidate_indices(route_key, count, key_group.indices())
            .into_iter()
            .next()
    }

    /// Reserve a backend for this request.
    ///
    /// Initial requests keep reusable-prefix affinity with bounded,
    /// key-decorrelated spillover. Once assistant or tool history is present,
    /// the stable first two messages select a conversation home instead. That
    /// allows at most one deterministic handoff after the first turn and keeps
    /// all later growing-history requests on the same backend.
    pub(super) fn acquire_index(
        &self,
        messages: &[crate::ChatMessage],
        pinned_pub_key: Option<&str>,
    ) -> Option<RouteLease> {
        let count = self.rotation_count();
        if count == 0 {
            return None;
        }
        let key_group = self.resolve_key_group(pinned_pub_key, count);
        if matches!(key_group, KeyGroup::UnknownKey) {
            self.warn_unknown_key(pinned_pub_key);
        }
        let allowed = key_group.indices();
        let has_history = has_conversation_history(messages);
        let route_key = self.route_key(messages);
        if messages.is_empty() {
            let index = allowed
                .and_then(|group| group.first())
                .copied()
                .unwrap_or(0);
            return Some(self.reserve_index(route_key, index));
        }
        let candidates = self.candidate_indices(route_key, count, allowed);
        if has_history {
            return Some(self.reserve_index(route_key, candidates[0]));
        }
        Some(self.acquire_candidates(route_key, count, &candidates))
    }

    /// Independent decision requests have no chat prefix. Hash the original
    /// request key and use the same bounded spillover as first-turn chat.
    pub(super) fn acquire_systemone_index(&self, request_hash: &str) -> Option<RouteLease> {
        let count = self.rotation_count();
        if count == 0 {
            return None;
        }
        let digest = Sha256::digest(request_hash.as_bytes());
        let route_key =
            u64::from_be_bytes(digest[..8].try_into().expect("SHA-256 has eight bytes"));
        let candidates = self.candidate_indices(route_key, count, None);
        Some(self.acquire_candidates(route_key, count, &candidates))
    }

    fn acquire_candidates(&self, route_key: u64, count: usize, candidates: &[usize]) -> RouteLease {
        let mut loads = lock(&self.prefix_loads);
        let counts = loads.entry(route_key).or_insert_with(|| vec![0; count]);
        if counts.len() < count {
            counts.resize(count, 0);
        }
        let index = candidates
            .iter()
            .copied()
            .find(|index| counts[*index] < PREFIX_AFFINITY_BURST)
            .unwrap_or_else(|| {
                candidates
                    .iter()
                    .enumerate()
                    .min_by_key(|(rank, index)| (counts[**index], *rank))
                    .map(|(_, index)| *index)
                    .expect("rotation count is non-zero")
            });
        counts[index] = counts[index].saturating_add(1);
        drop(loads);
        RouteLease {
            route_key,
            index,
            prefix_loads: self.prefix_loads.clone(),
        }
    }

    /// Reserve an explicit fallback index for the same prefix key.
    pub(super) fn reserve_index(&self, route_key: u64, index: usize) -> RouteLease {
        let mut loads = lock(&self.prefix_loads);
        let counts = loads.entry(route_key).or_default();
        if counts.len() <= index {
            counts.resize(index + 1, 0);
        }
        counts[index] = counts[index].saturating_add(1);
        drop(loads);
        RouteLease {
            route_key,
            index,
            prefix_loads: self.prefix_loads.clone(),
        }
    }

    #[cfg(test)]
    pub(super) fn active_prefix_loads(&self) -> usize {
        lock(&self.prefix_loads).len()
    }

    fn route_key(&self, messages: &[crate::ChatMessage]) -> u64 {
        if has_conversation_history(messages) {
            self.prefix_router.route_conversation(messages)
        } else {
            self.prefix_router.route(messages)
        }
    }

    pub(super) fn candidate_indices(
        &self,
        route_key: u64,
        count: usize,
        allowed: Option<&[usize]>,
    ) -> Vec<usize> {
        let (preferred, indices) = match allowed {
            Some(group) if !group.is_empty() => (
                group[(route_key % group.len() as u64) as usize],
                group.to_vec(),
            ),
            Some(_) | None => (
                (route_key % count as u64) as usize,
                (0..count).collect::<Vec<_>>(),
            ),
        };
        let stats = lock(&self.backend_stats);
        // Warmed EMA for index `i`, or None if out of range / not yet warmed.
        // `.get()` keeps this panic-free regardless of how `count` relates to
        // the stats vec length (it is `MAX_FANOUT` by construction).
        let ema = |i: usize| -> Option<f64> {
            stats
                .get(i)
                .filter(|s| s.samples >= TTFT_WARMUP_SAMPLES && s.ttft_ewma_ms > 0.0)
                .map(|s| s.ttft_ewma_ms)
        };
        let min_warm = indices
            .iter()
            .copied()
            .filter_map(ema)
            .fold(f64::MAX, f64::min);
        let is_slow = |index: usize| {
            ema(index).is_some_and(|value| {
                value > TTFT_SLOW_FLOOR_MS
                    && min_warm.is_finite()
                    && value > TTFT_SLOW_RATIO * min_warm
            })
        };
        let mut candidates: Vec<usize> = indices
            .into_iter()
            .filter(|index| !is_slow(*index))
            .collect();
        // Preserve the modulo primary, but give each colliding routing key a
        // different deterministic spill order. A simple ring makes all keys
        // with the same primary pile onto the same secondary under pressure.
        candidates.sort_by_key(|index| {
            (
                *index != preferred,
                Reverse(route_index_score(route_key, *index)),
                *index,
            )
        });
        if candidates.is_empty() {
            candidates.push(preferred);
        } else if is_slow(preferred) {
            // Preserve the existing behavior: when the deterministic primary
            // is pathological, lead with the fastest warmed healthy backend.
            if let Some(fastest) = candidates.iter().copied().min_by(|a, b| {
                ema(*a)
                    .unwrap_or(f64::MAX)
                    .partial_cmp(&ema(*b).unwrap_or(f64::MAX))
                    .unwrap_or(std::cmp::Ordering::Equal)
            }) {
                candidates.retain(|index| *index != fastest);
                candidates.insert(0, fastest);
            }
        }
        candidates
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
    ///
    /// When the request carries a pinned model pubkey and discovery resolved a
    /// key group for it, candidates are restricted to that group: a backend
    /// outside the group holds a different KMS-root-derived keypair and would
    /// fail to decrypt, turning a retryable 5xx into a misleading
    /// `400 "Decryption failed"`. An exhausted group yields an empty list, and
    /// the caller then surfaces the original 5xx.
    pub(super) fn fallback_indices_for(
        &self,
        tried: usize,
        pinned_pub_key: Option<&str>,
    ) -> Vec<usize> {
        let count = self.rotation_count();
        let key_group = self.resolve_key_group(pinned_pub_key, count);
        let stats = lock(&self.backend_stats);
        let mut idxs: Vec<usize> = match key_group.indices() {
            Some(indices) => indices
                .iter()
                .copied()
                .filter(|&index| index != tried)
                .collect(),
            None => (0..count).filter(|&index| index != tried).collect(),
        };
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
            //
            // Only clear clients in verifier mode: there, a `None` slot is
            // lazily re-verified on next use. In legacy/test mode (no verifier)
            // the slots are eagerly pre-created and there is nothing to re-create
            // them — clearing would wedge the provider with "no backend verifier
            // configured". Those legacy clients aren't backend-pinned by
            // attestation anyway, so leaving them is correct.
            if self.backend_verifier.is_some() {
                for slot in &self.index_clients {
                    *lock(slot) = None;
                }
            }
            // Index `i` may now reach a different backend.
            for failed_at in &self.channel_binding_failed_at {
                *lock(failed_at) = None;
            }
            let mut stats = lock(&self.backend_stats);
            for s in stats.iter_mut() {
                *s = BackendStat::default();
            }
            self.backend_keys
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
    }

    pub(super) fn set_backend_keys(&self, map: HashMap<String, Vec<usize>>) {
        let mut backend_keys = self.backend_keys.write().unwrap_or_else(|e| e.into_inner());
        if *backend_keys == map {
            return;
        }

        // Only clear clients in verifier mode: there, a `None` slot is
        // lazily re-verified on next use. In legacy/test mode (no verifier)
        // the slots are eagerly pre-created and there is nothing to re-create
        // them — clearing would wedge the provider with "no backend verifier
        // configured". Those legacy clients aren't backend-pinned by
        // attestation anyway, so leaving them is correct.
        if self.backend_verifier.is_some() {
            for slot in &self.index_clients {
                *lock(slot) = None;
            }
        }
        let mut stats = lock(&self.backend_stats);
        for stat in stats.iter_mut() {
            *stat = BackendStat::default();
        }
        *backend_keys = map;
    }

    pub(super) fn set_backend_hosts(&self, hosts: BackendHosts) {
        let hosts = Arc::new(hosts);
        self.backend_hosts.store(hosts.clone());
        if let Some(handles) = self.placement.load().as_ref() {
            handles.hosts.store(hosts);
        }
    }

    /// Install smart placement. The current host map is copied into
    /// `handles.hosts` so a map pushed before installation is not lost.
    ///
    /// Each handle set belongs to exactly one Fleet (its hosts `ArcSwap` must
    /// have one writer). Installing handles already installed elsewhere is a
    /// wiring bug: it is refused (this Fleet stays on the legacy path).
    pub(super) fn set_placement(&self, handles: PlacementHandles) {
        if !handles.io.claim_install() {
            tracing::warn!(
                error_kind = "already_installed",
                "Placement handles refused; this provider stays on the legacy path"
            );
            debug_assert!(
                false,
                "placement handles already installed on another Fleet"
            );
            return;
        }
        let handles = Arc::new(handles);
        // Seed the reader's map before publishing, then re-seed after: this
        // closes a check-then-act race with a concurrent `set_backend_hosts`.
        // If that call's newest map lands in `self.backend_hosts` (and, since
        // `self.placement` isn't populated yet, is not mirrored into
        // `handles.hosts`) strictly between the check above and a single
        // post-publish copy, the single copy could overwrite the fresh map
        // with a stale snapshot taken before it. Seeding twice means any
        // push that isn't picked up by the pre-publish copy is guaranteed to
        // be picked up by the post-publish one, since `set_backend_hosts`
        // itself starts mirroring into `handles.hosts` the moment
        // `self.placement` is populated by the `store` below.
        if !Arc::ptr_eq(&handles.hosts, &self.backend_hosts) {
            handles.hosts.store(self.backend_hosts.load_full());
        }
        self.placement.store(Some(handles.clone()));
        if !Arc::ptr_eq(&handles.hosts, &self.backend_hosts) {
            handles.hosts.store(self.backend_hosts.load_full());
        }
    }

    /// Place a covered-model request on the least-stalling host, or fall
    /// through to `acquire_index` unchanged. Placement is skipped entirely
    /// (no decision, log or metric) when it is not installed, the model is
    /// not covered, or rotation is unavailable. A `Legacy` decision, an
    /// incomplete host map or snapshot, an unmapped host, or a host outside
    /// the pinned E2EE key group all run the existing path.
    pub(super) fn acquire_index_placed(
        &self,
        messages: &[crate::ChatMessage],
        pinned_pub_key: Option<&str>,
        request: &PlacementRequest,
    ) -> Option<RouteLease> {
        if let Some(lease) = self.try_place(messages, pinned_pub_key, request) {
            return Some(lease);
        }
        self.acquire_index(messages, pinned_pub_key)
    }

    fn try_place(
        &self,
        messages: &[crate::ChatMessage],
        pinned_pub_key: Option<&str>,
        request: &PlacementRequest,
    ) -> Option<RouteLease> {
        if !COVERED_MODELS.contains(&request.model.as_str()) {
            return None;
        }
        let guard = self.placement.load();
        let handles = guard.as_ref()?;
        let count = self.rotation_count();
        if count == 0 {
            return None;
        }
        let now_ms = epoch_ms();
        let now_s = now_ms / 1_000;
        let input = PlaceInput {
            model: request.model.clone(),
            prompt_tokens_est: prompt_tokens_est(messages),
            affinity: request.affinity.clone(),
            affinity_source: request.affinity_source,
            // Context tiers are separate providers (pool-level), so this
            // Fleet has no host-level long-context knowledge: long prompts
            // are NoneEligible and fail open to the legacy path.
            long_context_hosts: Vec::new(),
            now_ms,
        };
        let key_group = self.resolve_key_group(pinned_pub_key, count);

        // `snapshot` and `host_map_incomplete` are read-only against
        // lock-free state (an `ArcSwap` load and this Fleet's own backend
        // map), independent of the placement ledger, so both are computed
        // before taking the ledger lock below.
        let snapshot = handles.io.snapshot.load();
        let incomplete = self.host_map_incomplete(&snapshot);

        // One critical section from reading this node's own pending load to
        // reserving the chosen host in it, so concurrent requests on this
        // node each see the others' reservations and do not herd onto the
        // same "least loaded" host. `place()` is synchronous and O(replicas),
        // and nothing here awaits, so holding the ledger lock across just
        // that path (mine_in -> place -> index_for_host -> ledger_add) is
        // cheap and is the only way to make concurrent requests on this node
        // observe each other's reservations. Logging and Valkey writes
        // happen after the lock is released.
        let placed = {
            let mut ledger = lock(&self.placement_ledger);
            let mine = mine_in(&ledger, now_s);
            let decision = handles
                .placer
                .place(&input, &snapshot, &mine, &mut rand::rng());
            match decision {
                Decision::Legacy { record, .. } => Err(record),
                Decision::Place { mut record, .. } if incomplete => {
                    demote(&mut record, LegacyReason::Incomplete);
                    Err(record)
                }
                Decision::Place {
                    host,
                    mut record,
                    pin_write,
                } => match self.index_for_host(&host, count) {
                    None => {
                        demote(&mut record, LegacyReason::HostUnmapped);
                        Err(record)
                    }
                    Some(index)
                        if key_group
                            .indices()
                            .is_some_and(|group| !group.contains(&index)) =>
                    {
                        demote(&mut record, LegacyReason::KeyGroup);
                        Err(record)
                    }
                    Some(index) => {
                        ledger_add(&mut ledger, host.clone(), input.prompt_tokens_est, now_s);
                        Ok((host, index, record, pin_write))
                    }
                },
            }
        };
        let (host, index, record, pin_write) = match placed {
            Ok(placed) => placed,
            Err(record) => {
                report_decision(handles, &record, request);
                return None;
            }
        };
        if matches!(key_group, KeyGroup::UnknownKey) {
            self.warn_unknown_key(pinned_pub_key);
        }

        let lease = self.reserve_index(self.route_key(messages), index);
        handles.io.record(Write::Routed {
            host,
            replica: None,
            tok: input.prompt_tokens_est,
            sec: now_s,
        });
        if let Some((pin_id, pin_host)) = pin_write {
            handles.io.record(Write::Pin {
                id_hex: pin_id.to_hex(),
                host: pin_host,
                at_ms: now_ms,
            });
        }
        report_decision(handles, &record, request);
        Some(lease)
    }

    /// True when placing would starve hosts the placer cannot see: the host
    /// map covers fewer hosts than this Fleet has backends (a partial proxy
    /// rollout, or indices without a replica key), or a mapped host has no
    /// replica view in `snapshot` (e.g. a restarted proxy whose new key is
    /// not discovered yet). O(hosts + replicas); no allocation beyond one set.
    fn host_map_incomplete(&self, snapshot: &Snapshot) -> bool {
        let hosts = self.backend_hosts.load();
        if hosts.index_by_host.len() < self.backend_count() {
            return true;
        }
        let seen: HashSet<&str> = snapshot
            .replicas
            .iter()
            .map(|v| v.host_id.as_str())
            .collect();
        hosts
            .index_by_host
            .keys()
            .any(|host| !seen.contains(host.as_str()))
    }

    /// The backend index `host` is bound to, when the pushed host map was
    /// built for this Fleet's current backend count (otherwise the binding
    /// is stale) and the index is within the rotation fan-out.
    fn index_for_host(&self, host: &str, count: usize) -> Option<usize> {
        let hosts = self.backend_hosts.load();
        if hosts.count != self.backend_count() {
            return None;
        }
        hosts
            .index_by_host
            .get(host)
            .copied()
            .filter(|index| *index < count)
    }

    /// This node's own placed load per host over `{now_s - 1, now_s}`.
    #[cfg(test)]
    pub(super) fn placement_mine(&self, now_s: u64) -> HashMap<String, Pending> {
        mine_in(&lock(&self.placement_ledger), now_s)
    }

    /// Rate-limited warning for a pinned key that discovery does not know.
    fn warn_unknown_key(&self, pinned_pub_key: Option<&str>) {
        if self.should_warn_unknown_key(epoch_ms()) {
            let pub_key_prefix: String = pinned_pub_key
                .unwrap_or_default()
                .chars()
                .take(16)
                .collect();
            tracing::warn!(
                pub_key_prefix = %pub_key_prefix,
                "No backend key group found for pinned model public key; routing unrestricted"
            );
        }
    }

    pub(super) fn should_warn_unknown_key(&self, now_ms: u64) -> bool {
        let mut last_warn_ms = self.last_unknown_key_warn_ms.load(Ordering::Relaxed);
        loop {
            if last_warn_ms != 0
                && now_ms.saturating_sub(last_warn_ms) < UNKNOWN_KEY_WARN_INTERVAL_MS
            {
                return false;
            }
            match self.last_unknown_key_warn_ms.compare_exchange(
                last_warn_ms,
                now_ms,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => last_warn_ms = observed,
            }
        }
    }

    fn resolve_key_group(&self, pinned_pub_key: Option<&str>, count: usize) -> KeyGroup {
        match pinned_pub_key.filter(|_| key_affinity_enabled()) {
            Some(pub_key) => self.key_group(pub_key, count),
            None => KeyGroup::Unrestricted,
        }
    }

    /// Return the routing policy for `pub_key`, bounded by the live count.
    pub(super) fn key_group(&self, pub_key: &str, count: usize) -> KeyGroup {
        let backend_keys = self.backend_keys.read().unwrap_or_else(|e| e.into_inner());
        if backend_keys.is_empty() {
            return KeyGroup::Unrestricted;
        }
        let normalized = pub_key.to_lowercase();
        let Some(group) = backend_keys.get(&normalized) else {
            return KeyGroup::UnknownKey;
        };
        let filtered: Vec<usize> = group
            .iter()
            .copied()
            .filter(|index| *index < count)
            .collect();
        if filtered.is_empty() {
            KeyGroup::UnknownKey
        } else {
            KeyGroup::Indices(filtered)
        }
    }

    /// Latest healthy backend count (best-effort, `Relaxed`).
    pub(super) fn backend_count(&self) -> usize {
        self.last_backend_count.load(Ordering::Relaxed)
    }
}

fn has_conversation_history(messages: &[crate::ChatMessage]) -> bool {
    messages.iter().skip(1).any(|message| {
        matches!(
            message.role,
            crate::MessageRole::Assistant | crate::MessageRole::Tool
        )
    })
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed
                .as_secs()
                .saturating_mul(1_000)
                .saturating_add(u64::from(elapsed.subsec_millis()))
        })
}

/// This node's own placed load per host over `{now_s - 1, now_s}`.
fn mine_in(ledger: &PlacementLedger, now_s: u64) -> HashMap<String, Pending> {
    let mut mine: HashMap<String, Pending> = HashMap::new();
    let window = now_s.saturating_sub(1)..=now_s;
    for (_, hosts) in ledger.iter().filter(|(sec, _)| window.contains(*sec)) {
        for (host, pending) in hosts {
            let entry = mine.entry(host.clone()).or_default();
            entry.req = entry.req.saturating_add(pending.req);
            entry.tok = entry.tok.saturating_add(pending.tok);
        }
    }
    mine
}

/// Reserves one placed request of `tok` tokens on `host` in second `now_s`,
/// dropping seconds that left the window.
fn ledger_add(ledger: &mut PlacementLedger, host: String, tok: u64, now_s: u64) {
    ledger.retain(|sec, _| sec.saturating_add(1) >= now_s);
    let pending = ledger.entry(now_s).or_default().entry(host).or_default();
    pending.req = pending.req.saturating_add(1);
    pending.tok = pending.tok.saturating_add(tok);
}

/// Turns a `Place` record into the `Legacy` one the caller fell back with.
fn demote(record: &mut DecisionRecord, reason: LegacyReason) {
    record.outcome = "legacy";
    record.reason = Some(reason.as_str());
    record.selection = None;
    record.rank = None;
    // The candidate was rejected: nothing below may describe a host this
    // request did not use, or `METRIC_CHOSEN_BACKLOG` ("chosen host, per
    // placed request") samples a record that was never placed. `eligible`
    // and `excluded` are left as-is: they still describe the scoring that
    // was actually done. Matches the shape of `DecisionRecord::legacy`.
    record.host = None;
    record.home = None;
    record.pinned = None;
    record.chosen_score = None;
    record.home_score = None;
    record.best_score = None;
    record.pending_req = 0;
    record.chosen_backlog_tokens = None;
}

fn route_index_score(route_key: u64, index: usize) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(route_key.to_be_bytes());
    hasher.update((index as u64).to_be_bytes());
    let digest = hasher.finalize();
    u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("SHA-256 digest always contains eight bytes"),
    )
}

pub(super) fn key_affinity_value_enabled(value: Option<&str>) -> bool {
    !matches!(value, Some("0" | "false" | "FALSE"))
}

/// Set `E2EE_BACKEND_KEY_AFFINITY=0` (or `false`) to disable pubkey-to-backend
/// affinity and restore pre-change routing. Default: enabled.
fn key_affinity_enabled() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| {
        let value = std::env::var("E2EE_BACKEND_KEY_AFFINITY").ok();
        key_affinity_value_enabled(value.as_deref())
    })
}

#[cfg(test)]
mod demote_tests {
    use super::*;
    use placement::rules::RULES;

    #[test]
    fn demote_clears_the_rejected_candidate_shape() {
        // Given: a fully populated `Place` decision, as if the placer had
        // chosen a host, right before the caller demotes it to legacy.
        let mut record = DecisionRecord {
            outcome: "place",
            reason: None,
            rank: Some(0),
            affinity: "prefix",
            selection: Some("primary"),
            host: Some("host-a".to_string()),
            home: Some("host-a".to_string()),
            pinned: Some("pubkey-a".to_string()),
            eligible: 3,
            excluded: RULES.map(|r| (r, 0)),
            chosen_score: Some(1.0),
            home_score: Some(1.0),
            best_score: Some(1.0),
            snapshot_age_ms: 42,
            pending_req: 5,
            chosen_backlog_tokens: Some(1234),
        };

        // When: the caller falls back to the legacy path.
        demote(&mut record, LegacyReason::HostUnmapped);

        // Then: the record matches the shape of a placer-built `Legacy`
        // record — no host, no backlog, nothing describing a candidate this
        // request never used.
        assert_eq!(record.outcome, "legacy");
        assert_eq!(record.reason, Some(LegacyReason::HostUnmapped.as_str()));
        assert_eq!(record.selection, None);
        assert_eq!(record.rank, None);
        assert_eq!(record.host, None);
        assert_eq!(record.home, None);
        assert_eq!(record.pinned, None);
        assert_eq!(record.chosen_score, None);
        assert_eq!(record.home_score, None);
        assert_eq!(record.best_score, None);
        assert_eq!(record.pending_req, 0);
        assert_eq!(record.chosen_backlog_tokens, None);
        // `eligible` and `excluded` are left untouched: they still describe
        // the scoring that was actually done.
        assert_eq!(record.eligible, 3);
    }
}
