use crate::attestation::AttestationVerifier;
use crate::common::encryption_headers;
use config::ExternalProvidersConfig;
use inference_providers::nearai;
use inference_providers::rotation;
use inference_providers::spki_verifier::{FingerprintState, SharedTlsRoots};
use inference_providers::{
    is_client_audio_input_status,
    models::{AttestationError, CompletionError},
    AudioTranscriptionError, AudioTranscriptionParams, AudioTranscriptionResponse,
    ChatCompletionParams, ExternalProvider, ExternalProviderConfig, ImageEditError,
    ImageEditParams, ImageEditResponseWithBytes, ImageGenerationError, ImageGenerationParams,
    ImageGenerationResponseWithBytes, InferenceProvider, ProviderConfig, RerankError, RerankParams,
    RerankResponse, StreamingResult, StreamingResultExt,
};
use regex::Regex;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

type InferenceProviderTrait = dyn InferenceProvider + Send + Sync;

/// Upper bound on leading SSE control events (keepalive comments, blank
/// lines — chunk-less `SSEEvent`s) consumed while peeking for the first
/// parsed chunk to establish sticky-routing. Real upstreams emit zero before
/// the first data chunk; the cap stops a misbehaving upstream from stalling
/// stream return or growing the stash unbounded (issue #701).
const MAX_LEADING_CONTROL_EVENTS: usize = 32;

/// Trait for fetching external model configurations from a data source (e.g., database).
/// This decouples the InferenceProviderPool from the database crate (hexagonal architecture).
#[async_trait::async_trait]
pub trait ExternalModelsSource: Send + Sync {
    async fn fetch_external_models(&self) -> Result<Vec<(String, serde_json::Value)>, String>;

    /// Fetch models that have a direct inference URL configured.
    /// Returns (model_name, inference_url) pairs for active models with inference_url set.
    /// These models are routed directly to the URL, bypassing the discovery server.
    async fn fetch_inference_url_models(&self) -> Result<Vec<(String, String)>, String>;
}

/// Result of an attestation-discovery pass against a model URL.
///
/// `discover_model` mutates the shared `fingerprint_state` as a side effect
/// (pinning verified fingerprints — additively on partial coverage, replacing
/// the set on complete coverage). This struct summarizes what happened for
/// the caller's logging and decision-making.
#[derive(Debug)]
struct DiscoveryOutcome {
    /// Healthy backend count reported by `GET /backends/count` this cycle.
    /// `0` means we couldn't get a count (model-proxy unreachable, 404, etc.)
    /// — see `failure_reasons` for the category. `discover_model` returns
    /// without issuing any rotation calls in that case.
    backend_count: usize,
    /// Number of discovery HTTP calls that returned a response.
    successful_calls: usize,
    /// Number of discovery HTTP calls that failed (timeout, transport error, 4xx/5xx).
    failed_calls: usize,
    /// Number of previously-unknown verified fingerprints added to `fingerprint_state`
    /// during this pass.
    new_fingerprints: usize,
    /// Total pinned fingerprints in `fingerprint_state` after this pass.
    total_pinned: usize,
    /// Signing pubkeys extracted from verified reports, keyed by signing algorithm
    /// ("ecdsa" / "ed25519"). Pubkeys are derived from the TEE compose hash so
    /// they're identical across backends of the same model.
    pubkeys_by_algo: HashMap<String, String>,
    /// Per-call verified TLS fingerprints observed in this pass, in launch
    /// order (`futures::future::join_all` preserves input order, not
    /// completion order). One entry per call, not per backend, so under
    /// complete coverage `observed_fingerprints.len() == max(backend_count,
    /// ALGOS.len())`. When `backend_count == 1`, the two algo calls hit
    /// the same backend and entries repeat — the set of *distinct*
    /// fingerprints is `verified_this_round` (used for pin updates).
    observed_fingerprints: Vec<String>,
    /// Per-call failure reasons that prevented a fingerprint observation, in
    /// launch order. Each entry is `"{category}: {detail}"` where category
    /// is one of: `count_connect`, `count_timeout`, `count_send`,
    /// `count_status`, `count_decode`, `client_build`, `query_encode`,
    /// `connect`, `send_timeout`, `request`, `send`, `timeout`, `status`,
    /// `malformed_json`, `verify`.
    /// Note: post-HTTP verify failures are included here even though the
    /// underlying call succeeded HTTP-wise, so
    /// `failure_reasons.len() == failed_calls + verify_failures` plus at
    /// most one `count_*` entry per cycle.
    failure_reasons: Vec<String>,
    /// Number of HTTP-successful calls whose attestation verification failed
    /// (TDX quote rejection, report-data mismatch, etc.). These are *not*
    /// counted in `failed_calls`, which only covers transport-layer failures.
    verify_failures: usize,
    /// True when this cycle achieved complete coverage of every healthy
    /// backend (no failures, every index produced a verified fingerprint,
    /// no duplicate fingerprints across indices) and the pin set was
    /// REPLACED rather than augmented. Lets a backend that went unhealthy
    /// or had its cert rotated drop out of the pin set within one refresh
    /// interval. `false` on any partial cycle to avoid evicting fingerprints
    /// we just couldn't reconfirm.
    replaced_state: bool,
}

/// Outcome of applying the cycle's verified fingerprints to a
/// `FingerprintState`. Split into its own type so the policy is testable
/// without spinning up a real attestation pipeline.
struct PinUpdate {
    /// Fingerprints that weren't in the pin set before this cycle.
    newly_pinned: Vec<String>,
    /// Fingerprints that were pinned before this cycle but are no longer
    /// pinned after the replacement. Empty in the additive path.
    evicted: Vec<String>,
    /// Pinned count after the update.
    total_pinned: usize,
    /// True iff the cycle achieved complete coverage and the pin set was
    /// replaced wholesale (vs. additively merged).
    replaced: bool,
}

/// Apply this cycle's verified fingerprint set to the shared pin state.
///
/// Rule: replace the pin set with `verified` iff this cycle achieved
/// **complete coverage** — every healthy backend produced exactly one
/// verified fingerprint and no failures occurred. Otherwise additively
/// merge (only grow). This is what lets a backend that just went unhealthy
/// drop out of the pin set within one refresh interval, without false
/// evictions on transient per-call hiccups.
fn apply_pin_update(
    state: &Arc<std::sync::RwLock<FingerprintState>>,
    verified: &HashSet<String>,
    backend_count: usize,
    failed_calls: usize,
    verify_failures: usize,
) -> PinUpdate {
    let complete_coverage = backend_count > 0
        && failed_calls == 0
        && verify_failures == 0
        && verified.len() == backend_count;

    let mut state = state.write().unwrap_or_else(|e| e.into_inner());
    let before: HashSet<String> = match &*state {
        FingerprintState::Pinned(set) => set.clone(),
        _ => HashSet::new(),
    };

    if complete_coverage {
        let newly_pinned: Vec<String> = verified.difference(&before).cloned().collect();
        let evicted: Vec<String> = before.difference(verified).cloned().collect();
        state.replace_with(verified.clone());
        PinUpdate {
            newly_pinned,
            evicted,
            total_pinned: state.pinned_count(),
            replaced: true,
        }
    } else {
        let mut newly_pinned: Vec<String> = Vec::new();
        for fp in verified {
            let before_count = state.pinned_count();
            state.add_fingerprint(fp.clone());
            if state.pinned_count() > before_count {
                newly_pinned.push(fp.clone());
            }
        }
        PinUpdate {
            newly_pinned,
            evicted: Vec::new(),
            total_pinned: state.pinned_count(),
            replaced: false,
        }
    }
}

/// Combined provider mappings updated atomically to prevent race conditions
/// Both mappings are updated together under a single lock to ensure consistency
#[derive(Clone)]
struct ProviderMappings {
    /// Map of model name -> list of providers (for load balancing)
    model_to_providers: HashMap<String, Vec<Arc<InferenceProviderTrait>>>,
    /// Map of model signing public key -> list of providers (for load balancing when multiple instances share the same key)
    pubkey_to_providers: HashMap<String, Vec<Arc<InferenceProviderTrait>>>,
}

impl ProviderMappings {
    fn new() -> Self {
        Self {
            model_to_providers: HashMap::new(),
            pubkey_to_providers: HashMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct InferenceProviderPool {
    /// Optional API key for authenticating with inference backends
    api_key: Option<String>,
    /// All providers (inference_url + external), updated atomically
    provider_mappings: Arc<RwLock<ProviderMappings>>,
    /// Configuration for external providers (API keys, timeouts, etc.)
    external_configs: ExternalProvidersConfig,
    /// Round-robin index for each model.
    /// Uses std::sync::RwLock because operations are instant HashMap lookups/inserts.
    load_balancer_index: Arc<std::sync::RwLock<HashMap<String, usize>>>,
    /// Map of chat_id -> provider for sticky routing
    chat_id_mapping: Arc<RwLock<HashMap<String, Arc<InferenceProviderTrait>>>>,
    /// Background task handle for periodic provider refresh from database
    refresh_task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Per-provider consecutive failure count, keyed by Arc pointer address.
    /// Providers with high failure counts are deprioritized in load balancing.
    /// Counts reset to 0 on success and are cleaned up on refresh.
    /// Uses std::sync::RwLock (not tokio) because all operations are non-blocking
    /// HashMap lookups/inserts — no .await while holding the lock.
    provider_failure_counts: Arc<std::sync::RwLock<HashMap<usize, u32>>>,
    /// Cache of inference_url → serving provider. When a model's URL hasn't changed
    /// across refreshes, the existing provider (and its warm reqwest::Client with
    /// pooled TLS connections) is reused instead of creating a new one.
    inference_url_providers: Arc<RwLock<HashMap<String, Arc<InferenceProviderTrait>>>>,
    /// Per-URL TLS fingerprint state, shared with the serving provider for that URL.
    /// Updated by discovery (both initial and cumulative) so new backend fingerprints
    /// are added over time without replacing the serving provider. Present only for
    /// URLs whose serving provider was created by this pool via `load_inference_url_models`.
    inference_url_fingerprint_states:
        Arc<RwLock<HashMap<String, Arc<std::sync::RwLock<FingerprintState>>>>>,
    /// Shared rustls root store + crypto provider, loaded once at pool creation.
    /// Attestation discovery uses this to build minimal `reqwest::Client`s without
    /// re-parsing ~150KB of native cert DER per call.
    tls_roots: SharedTlsRoots,
    /// Attestation verifier for TDX quote, GPU evidence, and image hash verification.
    attestation_verifier: Arc<AttestationVerifier>,
    /// Models registered out-of-band (not from the DB-backed discovery sources),
    /// e.g. the config-pinned Chutes provider. These are excluded from
    /// `remove_stale_providers` so a refresh tick — whose `valid_model_names` is
    /// built solely from the DB sources — does not wipe them.
    pinned_models: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    /// Out-of-band (config-pinned) providers keyed by model name — the source of
    /// truth for the pinned (attested third-party, e.g. Chutes) tier. On every
    /// discovery refresh the rebuilt provider list for a model becomes
    /// `[fresh discovered..] + [these]`, so a NEAR-served model refreshes its own
    /// backends while the attested Chutes fallback is never dropped or overwritten.
    /// Distinct from [`Self::pinned_models`] (just the name set the no-overwrite /
    /// no-evict / no-stale-remove guards consult).
    pinned_providers: Arc<std::sync::RwLock<HashMap<String, Vec<Arc<InferenceProviderTrait>>>>>,
    /// Optional metrics sink for tiered-routing visibility (set once after
    /// construction via [`Self::set_metrics_service`]; absent in tests). The pool
    /// is the only layer that knows which trust tier served a request and whether
    /// it was a fallback, so the per-tier / fallback counter is emitted from here.
    metrics_service: std::sync::OnceLock<Arc<dyn crate::metrics::MetricsServiceTrait>>,
}

/// Backend verifier that creates verified reqwest clients by connecting to a backend,
/// fetching its attestation report, and verifying the TDX quote + GPU evidence.
/// Used by `nearai::Provider` for lazy bucket client creation.
struct PoolBackendVerifier {
    api_key: Option<String>,
    model_name: String,
    tls_roots: SharedTlsRoots,
    attestation_verifier: Arc<AttestationVerifier>,
    /// Shared fingerprint state — newly discovered fingerprints are pinned here
    /// so other providers and discovery cycles benefit.
    fingerprint_state: Arc<std::sync::RwLock<FingerprintState>>,
}

#[async_trait::async_trait]
impl inference_providers::BackendVerifier for PoolBackendVerifier {
    async fn create_verified_client(&self, base_url: &str) -> Result<reqwest::Client, String> {
        // Fast path: if discovery has already pinned fingerprints for this
        // model's backends, skip the per-bucket attestation round-trip. The
        // shared `fingerprint_state` is updated every discovery cycle (~5 min)
        // with fresh GPU evidence; that cadence is the right freshness signal
        // for the attestation chain. Per-bucket re-attestation is redundant
        // work that adds ~1-3s of cold-bucket tail latency for no security
        // benefit — TLS SPKI pinning already proves backend identity continuity.
        //
        // The probe uses `GET /v1/models` (cheap static response) rather than
        // `/v1/attestation/report` (triggers backend-side GPU evidence
        // collection and signing).
        let pinned_snapshot = {
            let guard = self
                .fingerprint_state
                .read()
                .unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };
        let pinned_count = pinned_snapshot.pinned_count();
        if pinned_count > 0 {
            match self.try_pinned_fast_path(base_url, pinned_snapshot).await {
                Ok(client) => {
                    tracing::debug!(
                        pinned_count,
                        "Fast-path TLS probe succeeded, skipping attestation"
                    );
                    return Ok(client);
                }
                Err(reason) => {
                    tracing::debug!(
                        reason = %reason,
                        "Fast-path TLS probe failed, falling back to full attestation"
                    );
                }
            }
        }

        // Slow path: no pinned fingerprints yet, or the fast-path probe failed
        // (unknown backend, TLS rejection, or HTTP error). Run the full
        // attestation chain.
        //
        // 1. Build a client with isolated Bootstrap state (accepts any WebPKI cert
        //    for the initial connection to discover the backend's fingerprint).
        let client_state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
        let client = self
            .build_bucket_client(client_state.clone())
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

        // 2. Fetch attestation report — this establishes the H2 connection.
        //    Nonce must be 32-byte hex (same format as discover_model).
        let nonce_bytes: [u8; 32] = rand::random();
        let nonce = hex::encode(nonce_bytes);

        let qs = serde_urlencoded::to_string([
            ("model", self.model_name.as_str()),
            ("signing_algo", "ecdsa"),
            ("nonce", &nonce),
            ("include_tls_fingerprint", "true"),
        ])
        .map_err(|e| format!("Failed to build query string: {e}"))?;

        let url = format!("{base_url}/v1/attestation/report?{qs}");
        let mut request = client.get(&url);
        if let Some(ref key) = self.api_key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        let response = tokio::time::timeout(Duration::from_secs(10), request.send())
            .await
            .map_err(|_| "Attestation request timed out".to_string())?
            .map_err(|e| format!("Attestation request failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "unknown".to_string());
            return Err(format!("Attestation HTTP {status}: {body}"));
        }

        let report: serde_json::Map<String, serde_json::Value> = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse attestation response: {e}"))?;

        // 3. Verify the attestation report (TDX quote, GPU evidence, image hash).
        let verified = self
            .attestation_verifier
            .verify_attestation_report(&report, &nonce)
            .await
            .map_err(|e| format!("Attestation verification failed: {e}"))?;

        // 4. Pin the verified fingerprint in BOTH the shared state (so other
        //    providers benefit) AND the client's own state (so reconnections
        //    to a different backend are rejected — forces re-verification).
        if let Some(ref fp) = verified.tls_cert_fingerprint {
            // Shared state
            {
                let mut shared = self
                    .fingerprint_state
                    .write()
                    .unwrap_or_else(|e| e.into_inner());
                let before = shared.pinned_count();
                shared.add_fingerprint(fp.clone());
                if shared.pinned_count() > before {
                    info!(
                        fingerprint = %fp,
                        "Inline verification pinned new TLS fingerprint"
                    );
                }
            }
            // Client's own state: Bootstrap → Pinned({fp}).
            // If the H2 connection drops and reqwest silently reconnects,
            // the new handshake must match this specific fingerprint.
            // A reconnection to a different backend will fail, triggering
            // clear_bucket → re-verification.
            client_state
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .add_fingerprint(fp.clone());
        }

        // 5. Return the client — its H2 connection is to the verified backend,
        //    and its TLS verifier only accepts that backend on reconnection.
        Ok(client)
    }
}

impl PoolBackendVerifier {
    /// Build a bucket-flavored `reqwest::Client` with TLS verification driven
    /// by the supplied `FingerprintState`. Centralizing this here means the
    /// fast and slow paths can't drift on pool/timeout/keepalive settings.
    ///
    /// `read_timeout` is the per-chunk idle timeout; for non-streaming chat
    /// completion the connection sits silent the entire inference time, so it
    /// must match the configured completion budget — otherwise a long
    /// reasoning request fires `read_timeout` (~300s) before our `.timeout()`
    /// (default 600s). `VLLM_PROVIDER_COMPLETION_TIMEOUT` env override applies
    /// here too. `bucket_keepalive::apply` keeps the H2 connection sticky to
    /// a single backend across long idle gaps.
    fn build_bucket_client(
        &self,
        state: Arc<std::sync::RwLock<FingerprintState>>,
    ) -> Result<reqwest::Client, reqwest::Error> {
        let read_timeout =
            Duration::from_secs(nearai::Config::completion_timeout_from_env().max(0) as u64);
        let builder = reqwest::Client::builder()
            .use_preconfigured_tls(self.tls_roots.build_config(state))
            .pool_max_idle_per_host(1)
            .http2_adaptive_window(true)
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(read_timeout);
        inference_providers::bucket_keepalive::apply(builder).build()
    }

    /// Fast path for `create_verified_client` when discovery has already pinned
    /// fingerprints for this model's backends.
    ///
    /// Builds a client seeded with the snapshot of known-good fingerprints,
    /// then sends a cheap `GET /v1/models` request to validate via TLS handshake
    /// that the backend's cert SPKI is in the pinned set. On success, returns
    /// the established client without fetching attestation. On TLS rejection
    /// (unknown backend) or HTTP error, returns Err so the caller can fall
    /// back to the full attestation path.
    ///
    /// The H2 connection sits inside the returned client's pool. Subsequent
    /// requests on the same `reqwest::Client` reuse it. If the H2 connection
    /// drops and reqwest reconnects, the TLS verifier accepts any cert whose
    /// SPKI is in the snapshot — meaning a reconnect *can* land on a different
    /// attested backend. This is a deliberate relaxation of the prior
    /// "narrowed to one fingerprint per bucket" behavior: both backends are
    /// attested, so a cross-backend reconnect is secure even if it costs a
    /// prefix-cache miss on that one request. Avoiding the attestation chain
    /// on every cold-bucket-fill is worth that tradeoff.
    async fn try_pinned_fast_path(
        &self,
        base_url: &str,
        pinned_snapshot: FingerprintState,
    ) -> Result<reqwest::Client, String> {
        let client_state = Arc::new(std::sync::RwLock::new(pinned_snapshot));
        let client = self
            .build_bucket_client(client_state)
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

        // `/v1/models` is assumed to accept the same bearer token used for
        // inference (or to be unauthenticated). If a backend ever required a
        // different key here, the probe would 401 and we'd fall back to the
        // slow path — same outcome, just slower. The probe's value comes from
        // the TLS handshake, not the response body.
        let url = format!("{base_url}/v1/models");
        let mut request = client.get(&url);
        if let Some(ref key) = self.api_key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        // Wrap the entire probe — request send, status check, and body drain —
        // in a single 5-second timeout. A single budget is simpler and more
        // correct than two separate timeouts: any stall anywhere in the probe
        // should abort and fall through to the slow path within 5 s total.
        //
        // Body drain is required so reqwest can return the H2 stream to the
        // connection pool for the subsequent inference request. /v1/models
        // returns a tiny static payload (~1 KB) so this completes instantly
        // in practice.
        tokio::time::timeout(Duration::from_secs(5), async {
            let response = request
                .send()
                .await
                .map_err(|e| format!("Fast-path probe failed: {e}"))?;
            if !response.status().is_success() {
                let status = response.status();
                return Err(format!("Fast-path probe HTTP {status}"));
            }
            response
                .bytes()
                .await
                .map_err(|e| format!("Failed to drain probe body: {e}"))?;
            Ok(())
        })
        .await
        .map_err(|_| "Fast-path probe timed out".to_string())??;

        // No fingerprint is added to the shared `fingerprint_state` here —
        // the snapshot was already derived from it, so there is nothing new
        // to pin. Discovery (every ~5 min) remains the sole writer of the
        // shared state; contrast with the slow path's lines above that call
        // `shared.add_fingerprint` after a fresh attestation.
        Ok(client)
    }
}

impl InferenceProviderPool {
    /// Create a new pool with optional API key for backend authentication
    pub fn new(api_key: Option<String>, external_configs: ExternalProvidersConfig) -> Self {
        // Single source of truth for the PCCS endpoint: build the NEAR verifier
        // from the parsed config's `pccs_url` rather than re-reading `PCCS_URL`
        // from the environment, so it can't diverge from the Chutes verifier
        // (which is constructed from the same config field).
        let pccs_url = external_configs.pccs_url.clone();
        Self {
            api_key,
            provider_mappings: Arc::new(RwLock::new(ProviderMappings::new())),
            external_configs,
            load_balancer_index: Arc::new(std::sync::RwLock::new(HashMap::new())),
            chat_id_mapping: Arc::new(RwLock::new(HashMap::new())),
            refresh_task_handle: Arc::new(Mutex::new(None)),
            provider_failure_counts: Arc::new(std::sync::RwLock::new(HashMap::new())),
            inference_url_providers: Arc::new(RwLock::new(HashMap::new())),
            inference_url_fingerprint_states: Arc::new(RwLock::new(HashMap::new())),
            tls_roots: SharedTlsRoots::load(),
            attestation_verifier: Arc::new(AttestationVerifier::near_with_pccs(pccs_url)),
            pinned_models: Arc::new(std::sync::RwLock::new(std::collections::HashSet::new())),
            pinned_providers: Arc::new(std::sync::RwLock::new(HashMap::new())),
            metrics_service: std::sync::OnceLock::new(),
        }
    }

    /// Attach a metrics sink for tiered-routing/fallback visibility. Set once
    /// during app setup (the pool is shared as `Arc` immediately, so this uses
    /// interior mutability rather than a `new()` arg — keeping the many test
    /// `InferenceProviderPool::new(...)` call sites untouched). A second call is
    /// a no-op.
    pub fn set_metrics_service(&self, metrics: Arc<dyn crate::metrics::MetricsServiceTrait>) {
        let _ = self.metrics_service.set(metrics);
    }

    /// Load external providers (OpenAI, Anthropic, Gemini, etc.) into provider_mappings.
    pub async fn load_external_providers(
        &self,
        models: Vec<(String, serde_json::Value)>,
    ) -> Result<(), String> {
        let mut success_count = 0;
        let mut error_count = 0;

        let pinned = self
            .pinned_models
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut mappings = self.provider_mappings.write().await;
        for (model_name, provider_config) in models {
            // Never overwrite a pinned (out-of-band, attested) provider with a
            // DB-discovered external one (the external refresh routes here).
            if pinned.contains(&model_name) {
                warn!(model = %model_name, "Skipping external provider for a pinned (attested) model");
                continue;
            }
            match self.create_external_provider(&model_name, provider_config) {
                Ok((provider, backend_type)) => {
                    mappings
                        .model_to_providers
                        .insert(model_name.clone(), vec![provider]);
                    info!(model = %model_name, backend = %backend_type, "Registered external provider");
                    success_count += 1;
                }
                Err(e) => {
                    warn!(model = %model_name, error = %e, "Failed to register external provider");
                    error_count += 1;
                }
            }
        }

        info!(
            success = success_count,
            errors = error_count,
            "Loaded external providers"
        );

        if error_count > 0 && success_count == 0 {
            Err(format!(
                "All {} external provider(s) failed to load",
                error_count
            ))
        } else {
            Ok(())
        }
    }

    /// Check if a model has a registered provider
    pub async fn has_provider(&self, model_name: &str) -> bool {
        let mappings = self.provider_mappings.read().await;
        mappings.model_to_providers.contains_key(model_name)
    }

    /// Remove a provider by model name. Used when admin deactivates a model.
    /// Also cleans up pubkey_to_providers, load_balancer_index, and provider_failure_counts.
    pub async fn unregister_provider(&self, model_name: &str) -> bool {
        // If it was pinned, also clear the pin — otherwise DB discovery could
        // never re-register a model with this name (the insert guards skip pinned).
        self.pinned_models
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(model_name);
        // Drop any out-of-band (secondary) pinned providers too, so a later
        // discovery refresh doesn't re-attach a now-unregistered Chutes fallback.
        self.pinned_providers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(model_name);
        let mut mappings = self.provider_mappings.write().await;
        let removed_providers = mappings.model_to_providers.remove(model_name);
        if let Some(removed) = &removed_providers {
            // Prune pubkey entries pointing to the removed providers
            let removed_ptrs: std::collections::HashSet<usize> = removed
                .iter()
                .map(|p| Arc::as_ptr(p) as *const () as usize)
                .collect();
            mappings.pubkey_to_providers.retain(|_, providers| {
                providers
                    .retain(|p| !removed_ptrs.contains(&(Arc::as_ptr(p) as *const () as usize)));
                !providers.is_empty()
            });

            // Clean up load balancer index and failure counts for removed providers
            self.load_balancer_index
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(model_name);
            self.provider_failure_counts
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|key, _| !removed_ptrs.contains(key));

            info!(model = %model_name, "Unregistered provider");
        }
        removed_providers.is_some()
    }

    /// Register a provider for a model manually (useful for testing with mock providers)
    /// Also populates model_pub_key_mapping by fetching the attestation report
    /// Fetches attestation reports for both ECDSA and Ed25519 to support both signing algorithms
    pub async fn register_provider(&self, model_id: String, provider: Arc<InferenceProviderTrait>) {
        // Fetch signing public keys for both algorithms
        // Use "mock" as URL identifier for logging (since this is typically used for mock providers)
        let (pub_key_updates, _has_valid_attestation, _attestation_reports) =
            Self::fetch_signing_public_keys_for_both_algorithms(&provider, &model_id, "mock").await;

        // Atomic update: update both mappings together under a single lock
        let mut mappings = self.provider_mappings.write().await;
        mappings
            .model_to_providers
            .insert(model_id, vec![provider.clone()]);
        for (key, provider) in pub_key_updates {
            mappings
                .pubkey_to_providers
                .entry(key)
                .or_default()
                .push(provider);
        }
    }

    /// Reserve `model_ids` as pinned (verifiable) **before** any external/discovery
    /// load, WITHOUT attaching a provider. Fail-closed guard for configured Chutes
    /// canonical ids: marking them pinned makes [`Self::load_external_providers`]
    /// (and every periodic refresh) skip them, so a plaintext external/OpenRouter
    /// row sharing a canonical id can never register — even if the Chutes provider
    /// later fails to build (missing key / construction error). A reserved id with
    /// no attested provider then fails closed (404); it never silently serves
    /// plaintext for a model an operator configured as verifiable.
    pub fn reserve_pinned_models(&self, model_ids: &[String]) {
        let mut pinned = self
            .pinned_models
            .write()
            .unwrap_or_else(|e| e.into_inner());
        for id in model_ids {
            pinned.insert(id.clone());
        }
    }

    /// Register a pinned (out-of-band, e.g. config-Chutes) provider as a **secondary**
    /// under a model name that may also be served by DB-discovered (e.g. NEAR)
    /// providers — **pushing** onto the existing list rather than replacing it (a
    /// Chutes-only id ends up with just this provider, so it serves as primary). This
    /// is the sole pinned-registration path used in production.
    ///
    /// Used for the Chutes fallback tier: a NEAR-served canonical id keeps its own
    /// attested fleet as primary and gains Chutes as fallback. The provider is
    /// recorded in [`Self::pinned_providers`] so each discovery refresh re-attaches it
    /// after rebuilding the NEAR backends (never dropped/overwritten), and its name is
    /// added to [`Self::pinned_models`] so the discovery guards never evict it. Tier
    /// ordering (NEAR before Attested3p) is applied at selection time, so the push
    /// order here does not matter.
    ///
    /// Unlike [`Self::register_provider`], this does NOT run signing-key attestation
    /// discovery (a wasted round trip for Chutes, which has no signing-address pubkey
    /// and verifies its backend per request) and so does NOT populate
    /// `pubkey_to_providers`. A pubkey-routed (E2EE) request — selected via
    /// `model_pub_key` — therefore gets NO Chutes fallback by design: Chutes has no
    /// per-response signing key, its integrity is the ML-KEM AEAD channel itself.
    pub async fn register_pinned_secondary_provider(
        &self,
        model_id: String,
        provider: Arc<InferenceProviderTrait>,
    ) {
        let ptr = Arc::as_ptr(&provider) as *const () as usize;
        self.pinned_models
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(model_id.clone());
        {
            // Dedup by Arc pointer here too (symmetric with model_to_providers
            // below) so a repeated registration can't stack duplicate providers
            // that every refresh would re-attach.
            let mut pinned_providers = self
                .pinned_providers
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let entry = pinned_providers.entry(model_id.clone()).or_default();
            if !entry
                .iter()
                .any(|p| Arc::as_ptr(p) as *const () as usize == ptr)
            {
                entry.push(provider.clone());
            }
        }
        let mut mappings = self.provider_mappings.write().await;
        let entry = mappings.model_to_providers.entry(model_id).or_default();
        // Avoid a duplicate if this exact Arc is somehow already present.
        if !entry
            .iter()
            .any(|p| Arc::as_ptr(p) as *const () as usize == ptr)
        {
            entry.push(provider);
        }
    }

    /// Whether `model_name` is currently registered as a **pinned** (out-of-band)
    /// provider. The admin PATCH path uses this to avoid tearing down a pinned
    /// provider (e.g. Chutes): pinned providers are registered from config at
    /// startup and are not re-registered by the DB-discovery (inference-url /
    /// external) paths, so unregistering one on a metadata/activation update would
    /// leave an active catalog row with no serving provider until restart.
    pub fn is_pinned(&self, model_name: &str) -> bool {
        self.pinned_models
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(model_name)
    }

    /// Register multiple providers for multiple models (useful for testing)
    /// Also populates model_pub_key_mapping by fetching attestation reports
    /// Fetches attestation reports for both ECDSA and Ed25519 to support both signing algorithms
    pub async fn register_providers(&self, providers: Vec<(String, Arc<InferenceProviderTrait>)>) {
        // Phase 1: Collect attestation reports and public keys (no locks held)
        let mut pub_key_updates: Vec<(String, Arc<InferenceProviderTrait>)> = Vec::new();
        let mut model_providers: HashMap<String, Vec<Arc<InferenceProviderTrait>>> = HashMap::new();

        for (model_id, provider) in providers {
            // Fetch signing public keys for both algorithms to populate model_pub_key_mapping
            // Use "mock" as URL identifier for logging (since this is typically used for mock providers)
            let (keys, _has_valid_attestation, _attestation_reports) =
                Self::fetch_signing_public_keys_for_both_algorithms(&provider, &model_id, "mock")
                    .await;
            pub_key_updates.extend(keys);

            model_providers.entry(model_id).or_default().push(provider);
        }

        // Phase 2: Atomic bulk update of both mappings under a single lock
        // This ensures consistency - both mappings are updated together
        {
            let pinned = self
                .pinned_models
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let mut mappings = self.provider_mappings.write().await;
            for (model_id, providers) in model_providers {
                // Don't clobber a pinned (attested) provider (see register_pinned_secondary_provider).
                if pinned.contains(&model_id) {
                    warn!(model = %model_id, "Skipping register_providers for a pinned model");
                    continue;
                }
                mappings.model_to_providers.insert(model_id, providers);
            }
            for (key, provider) in pub_key_updates {
                mappings
                    .pubkey_to_providers
                    .entry(key)
                    .or_default()
                    .push(provider);
            }
        }
    }

    /// Fetch signing public keys for both ECDSA and Ed25519 algorithms
    ///
    /// Attempts to fetch attestation reports for both signing algorithms and returns
    /// all available signing public keys. Requests `include_tls_fingerprint=true` so
    /// the attestation binds the TLS certificate SPKI to the TDX report.
    ///
    /// # Arguments
    /// * `provider` - The inference provider to fetch the attestation reports from
    /// * `model_name` - The model name to request attestation for
    /// * `url` - Optional URL for logging purposes (can be empty string if not available)
    ///
    /// # Returns
    /// * Tuple of (signing_public_keys, has_valid_attestation, attestation_reports) where:
    ///   - `signing_public_keys`: Vector of (signing_public_key, provider) tuples for all available algorithms
    ///   - `has_valid_attestation`: True if at least one attestation report was successfully fetched
    ///   - `attestation_reports`: The raw attestation reports for further verification (TDX, GPU, image hash)
    async fn fetch_signing_public_keys_for_both_algorithms(
        provider: &Arc<InferenceProviderTrait>,
        model_name: &str,
        url: &str,
    ) -> (
        Vec<(String, Arc<InferenceProviderTrait>)>,
        bool,
        Vec<serde_json::Map<String, serde_json::Value>>,
    ) {
        let mut pub_key_updates = Vec::new();
        let mut has_valid_attestation = false;
        let mut attestation_reports = Vec::new();

        // Fetch for ECDSA
        if let Some(attestation_report) = Self::fetch_attestation_report_with_retry_for_algo(
            provider,
            model_name,
            url,
            Some("ecdsa"),
        )
        .await
        {
            has_valid_attestation = true;
            if let Some(signing_public_key) = attestation_report
                .get("signing_public_key")
                .and_then(|v| v.as_str())
            {
                pub_key_updates.push((signing_public_key.to_string(), provider.clone()));
            }
            attestation_reports.push(attestation_report);
        }

        // Fetch for Ed25519
        if let Some(attestation_report) = Self::fetch_attestation_report_with_retry_for_algo(
            provider,
            model_name,
            url,
            Some("ed25519"),
        )
        .await
        {
            has_valid_attestation = true;
            if let Some(signing_public_key) = attestation_report
                .get("signing_public_key")
                .and_then(|v| v.as_str())
            {
                pub_key_updates.push((signing_public_key.to_string(), provider.clone()));
            }
            attestation_reports.push(attestation_report);
        }

        (pub_key_updates, has_valid_attestation, attestation_reports)
    }

    /// Fetch attestation report with retries for a specific signing algorithm
    ///
    /// Retries up to 3 times with exponential backoff (100ms, 200ms, 400ms).
    /// This prevents providers from being excluded from the pool due to transient network issues.
    ///
    /// # Arguments
    /// * `provider` - The inference provider to fetch the attestation report from
    /// * `model_name` - The model name to request attestation for
    /// * `url` - Optional URL for logging purposes (can be empty string if not available)
    /// * `signing_algo` - Optional signing algorithm ("ecdsa" or "ed25519")
    ///
    /// # Returns
    /// * `Some(attestation_report)` if successful after retries
    /// * `None` if all retry attempts failed
    async fn fetch_attestation_report_with_retry_for_algo(
        provider: &Arc<InferenceProviderTrait>,
        model_name: &str,
        url: &str,
        signing_algo: Option<&str>,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        const MAX_ATTEMPTS: u32 = 3;
        const INITIAL_DELAY_MS: u64 = 100;

        for attempt in 0..MAX_ATTEMPTS {
            match provider
                .get_attestation_report(
                    model_name.to_string(),
                    signing_algo.map(|s| s.to_string()),
                    None,
                    None,
                    true,
                )
                .await
            {
                Ok(report) => {
                    if attempt > 0 {
                        tracing::debug!(
                            model = %model_name,
                            url = %url,
                            attempt = attempt + 1,
                            "Successfully fetched attestation report after retry"
                        );
                    }
                    return Some(report);
                }
                Err(e) => {
                    if attempt < MAX_ATTEMPTS - 1 {
                        // Exponential backoff: 100ms, 200ms, 400ms
                        let delay_ms = INITIAL_DELAY_MS * (1 << attempt);
                        tracing::debug!(
                            model = %model_name,
                            url = %url,
                            attempt = attempt + 1,
                            max_attempts = MAX_ATTEMPTS,
                            delay_ms = delay_ms,
                            error = %e,
                            "Failed to fetch attestation report, retrying..."
                        );
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    } else {
                        tracing::warn!(
                            model = %model_name,
                            url = %url,
                            attempts = MAX_ATTEMPTS,
                            error = %e,
                            "Provider failed to return attestation report after retries, excluding from pool"
                        );
                    }
                }
            }
        }

        None
    }

    /// Run attestation-discovery calls against a model URL, each on its own
    /// minimal `reqwest::Client` (fresh TCP connection, isolated `FingerprintState`),
    /// covering every (backend_index, signing_algo) pair needed to harvest
    /// both ECDSA and Ed25519 signing pubkeys in the same pass.
    ///
    /// The number of calls is `max(backend_count, ALGOS.len())`: one per
    /// backend (for TLS-cert fingerprint coverage across all serving CVMs)
    /// and at least one per algo (so both ECDSA and Ed25519 pubkeys are
    /// fetched even when a model has only a single backend). For
    /// `backend_count >= ALGOS.len()`, this degenerates to one call per
    /// backend; for `backend_count == 1` it issues two calls to the same
    /// backend, one per algo.
    ///
    /// Why a fresh client per call: reqwest with HTTP/2 multiplexes many
    /// concurrent requests onto a single TCP connection, which hashes to a
    /// single L4 backend. Separate clients force separate TCP handshakes,
    /// letting each call land on a different backend.
    ///
    /// Why rotation SNI per call: model-proxy publishes `<canonical>-i<N>.<base>`
    /// (see model-proxy PR #27). A fresh-TCP probe to that SNI is routed
    /// deterministically to `backends_sorted_by_address[N % healthy_count]`,
    /// bypassing the least-connections LB. We fetch the healthy count from
    /// `/backends/count`, then fan out calls across backend indices and
    /// algos. One cycle = full coverage.
    ///
    /// Single-backend floor: when `backend_count < ALGOS.len()`, the loop
    /// would otherwise miss an algorithm — e.g., `backend_count=1` would
    /// only fetch ECDSA and never harvest the Ed25519 pubkey, leaving
    /// `pubkey_to_providers` permanently missing that entry and breaking
    /// Ed25519 E2EE for that model (nearai/infra#167). We pad the iteration
    /// count to `max(backend_count, ALGOS.len())` so every algo is hit at
    /// least once; the rotation index wraps with `i % backend_count`, so
    /// the extra iterations re-probe an existing backend with the missing
    /// algo. Pubkeys are TEE-derived from the compose hash so the same
    /// backend serves a deterministic pubkey per algo.
    ///
    /// Why an isolated Bootstrap state per call: if discovery calls shared
    /// the caller's `fingerprint_state`, the first call that completed and
    /// pinned its backend's SPKI would transition the state to `Pinned({A})`,
    /// and peers hitting different backends would have their TLS handshakes
    /// rejected inside the SPKI verifier (fingerprint not in `{A}`). Each
    /// call therefore uses its own `Bootstrap` state for the TLS verifier,
    /// and verified fingerprints are merged into the caller's shared
    /// accumulator *after* the HTTP calls return.
    ///
    /// Why extract pubkeys here: the attestation report already contains
    /// `signing_public_key` for the requested `signing_algo`. The
    /// `max(backend_count, ALGOS.len())` fan-out guarantees both ECDSA and
    /// Ed25519 are queried at least once per cycle, even when a model has
    /// only a single backend. Pubkeys are derived from the TEE compose
    /// hash so they're identical across backends of the same model+algo;
    /// the first verified response per algo wins.
    ///
    /// Rapid eviction: when every healthy backend produced exactly one
    /// verified fingerprint, the pin set is REPLACED with the observed set
    /// — a backend that just went unhealthy or had its cert rotated is
    /// dropped from the pin set within one refresh interval. On partial
    /// coverage (any failure, or fewer distinct fingerprints than the
    /// reported healthy count) we fall back to additive merging so a
    /// transient hiccup doesn't evict verified fingerprints we just
    /// couldn't reconfirm.
    ///
    /// The caller owns the `fingerprint_state` Arc and decides when to
    /// transition it to `Blocked` (e.g., when `outcome.total_pinned == 0`
    /// on initial discovery).
    /// Build a DiscoveryOutcome representing "cycle bailed before issuing any
    /// rotation calls" (URL didn't parse, count fetch failed, etc.). Reads
    /// the current `total_pinned` from the shared state so callers see what
    /// remains pinned, but reports `successful_calls == 0` and never
    /// replaces the pin set.
    fn empty_outcome(
        fingerprint_state: &Arc<std::sync::RwLock<FingerprintState>>,
        backend_count: usize,
        failure_reasons: Vec<String>,
    ) -> DiscoveryOutcome {
        let total_pinned = fingerprint_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .pinned_count();
        DiscoveryOutcome {
            backend_count,
            successful_calls: 0,
            failed_calls: 0,
            new_fingerprints: 0,
            total_pinned,
            pubkeys_by_algo: HashMap::new(),
            observed_fingerprints: Vec::new(),
            failure_reasons,
            verify_failures: 0,
            replaced_state: false,
        }
    }

    async fn discover_model(
        url: &str,
        api_key: &Option<String>,
        model_name: &str,
        fingerprint_state: Arc<std::sync::RwLock<FingerprintState>>,
        tls_roots: &SharedTlsRoots,
        verifier: &AttestationVerifier,
    ) -> DiscoveryOutcome {
        const PER_CALL_TIMEOUT: Duration = Duration::from_secs(10);
        const COUNT_TIMEOUT: Duration = Duration::from_secs(3);
        const ALGOS: [&str; 2] = ["ecdsa", "ed25519"];

        /// Query parameters for `/v1/attestation/report`. Matches
        /// `nearai::Provider::get_attestation_report`'s Query struct; duplicated
        /// here so discovery doesn't need a full nearai::Provider (which spins
        /// up 128 bucket clients per instance — very heavy).
        #[derive(serde::Serialize)]
        struct Query<'a> {
            model: &'a str,
            signing_algo: Option<&'a str>,
            nonce: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            include_tls_fingerprint: Option<bool>,
        }

        let mut failure_reasons: Vec<String> = Vec::new();

        // Pre-flight: parse the URL into its rotation parts. If the URL
        // doesn't conform (one-label host, IP literal, etc.) we can't issue
        // rotation calls; record the reason and return a no-op outcome.
        let url_parsed = match url::Url::parse(url) {
            Ok(u) => u,
            Err(e) => {
                failure_reasons.push(format!("url_parse: {e}"));
                return Self::empty_outcome(&fingerprint_state, 0, failure_reasons);
            }
        };
        let parts = match rotation::split_inference_url(&url_parsed) {
            Some(p) => p,
            None => {
                failure_reasons.push("url_parse: host is not a child of a multi-label base".into());
                return Self::empty_outcome(&fingerprint_state, 0, failure_reasons);
            }
        };

        // Step 1: fetch the healthy backend count.
        //
        // The count endpoint terminates TLS at completions.near.ai (model-proxy
        // base domain) with a normal Let's Encrypt cert, so an unpinned
        // (Bootstrap) verifier is appropriate — there's no per-backend SPKI
        // to bind to here. We reuse the existing tls_roots so we don't build
        // yet another crypto provider.
        let count_state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
        let count_client = match reqwest::Client::builder()
            .use_preconfigured_tls(tls_roots.build_config(count_state))
            .connect_timeout(Duration::from_secs(3))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                failure_reasons.push(format!("count_client_build: {e}"));
                return Self::empty_outcome(&fingerprint_state, 0, failure_reasons);
            }
        };
        let backend_count =
            match rotation::fetch_backend_count(&count_client, &parts, COUNT_TIMEOUT).await {
                rotation::CountFetch::Ok(0) => {
                    // Authoritatively no healthy backends right now. Don't issue
                    // calls; don't replace the pin set (transient registry hiccup
                    // shouldn't evict verified state). Provider-level fail-closed
                    // paths handle the no-backend case at request time. Record
                    // `count_zero` so DD can distinguish this from a count-fetch
                    // failure (which would carry a `count_*:` reason instead).
                    failure_reasons
                        .push("count_zero: proxy reports 0 healthy backends".to_string());
                    return Self::empty_outcome(&fingerprint_state, 0, failure_reasons);
                }
                rotation::CountFetch::Ok(n) => n,
                rotation::CountFetch::Err(reason) => {
                    failure_reasons.push(reason);
                    return Self::empty_outcome(&fingerprint_state, 0, failure_reasons);
                }
            };

        // Defense-in-depth: cap the fan-out. A bogus registry reading (race
        // during a deploy, partial split) could otherwise spawn an unbounded
        // number of fresh-TCP TLS handshakes per cycle per model. Shared
        // with nearai::Provider's traffic-time rotation gate so the cap is
        // defined exactly once.
        let backend_count = if backend_count > rotation::MAX_FANOUT {
            warn!(
                model = %model_name,
                url = %url,
                reported = backend_count,
                capped_at = rotation::MAX_FANOUT,
                "backend count from proxy exceeds sanity cap, truncating fan-out"
            );
            failure_reasons.push(format!(
                "count_capped: proxy reported {backend_count} > {}",
                rotation::MAX_FANOUT
            ));
            rotation::MAX_FANOUT
        } else {
            backend_count
        };

        // Step 2: fan out attestation calls across (backend_index, algo) pairs
        // in parallel (no stagger). Total calls = max(backend_count,
        // ALGOS.len()) so every algo is sampled at least once even when a
        // model has only a single backend (which would otherwise leave one
        // algo's pubkey out of pubkey_to_providers, breaking E2EE routing
        // for that algo — see nearai/cloud-api#710).
        //
        // backend_index = i % backend_count maps the call sequence back to a
        // rotation backend; for backend_count >= 2 this equals i and the
        // loop degenerates to one call per backend (unchanged from before).
        let call_count = backend_count.max(ALGOS.len());
        let futures = (0..call_count)
            .map(|i| {
                let backend_index = i % backend_count;
                let parts = parts.clone();
                let api_key = api_key.clone();
                let model = model_name.to_string();
                let tls_roots = tls_roots.clone();
                let algo = ALGOS[i % ALGOS.len()].to_string();
                async move {
                    // Isolated Bootstrap state per call — see function doc.
                    let local_state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
                    let rustls_config = tls_roots.build_config(local_state);

                    let client = match reqwest::Client::builder()
                        .use_preconfigured_tls(rustls_config)
                        .connect_timeout(Duration::from_secs(5))
                        .read_timeout(PER_CALL_TIMEOUT)
                        .build()
                    {
                        Ok(c) => c,
                        Err(e) => {
                            debug!(
                                model = %model,
                                index = backend_index,
                                algo = %algo,
                                error = %e,
                                "Failed to build discovery client"
                            );
                            return Err(format!("client_build: {e}"));
                        }
                    };

                    let mut request_url =
                        match rotation::rotation_base_url(&parts, backend_index as u64) {
                            Some(u) => u,
                            None => return Err("rotation_url_build: failed".to_string()),
                        };
                    request_url.set_path("/v1/attestation/report");

                    let nonce_bytes: [u8; 32] = rand::random();
                    let nonce = hex::encode(nonce_bytes);
                    let qs = match serde_urlencoded::to_string(&Query {
                        model: &model,
                        signing_algo: Some(&algo),
                        nonce: Some(&nonce),
                        include_tls_fingerprint: Some(true),
                    }) {
                        Ok(q) => q,
                        Err(e) => return Err(format!("query_encode: {e}")),
                    };
                    request_url.set_query(Some(&qs));

                    let mut req = client.get(request_url.clone());
                    if let Some(key) = api_key.as_ref() {
                        req = req.header("Authorization", format!("Bearer {}", key));
                    }

                    let start = std::time::Instant::now();
                    let res = tokio::time::timeout(PER_CALL_TIMEOUT, req.send()).await;
                    let elapsed_ms = start.elapsed().as_millis() as u64;

                    let resp = match res {
                        Ok(Ok(r)) => r,
                        Ok(Err(e)) => {
                            debug!(
                                model = %model,
                                index = backend_index,
                                algo = %algo,
                                elapsed_ms,
                                error = %e,
                                "Discovery call failed"
                            );
                            let category = if e.is_connect() {
                                "connect"
                            } else if e.is_timeout() {
                                "send_timeout"
                            } else if e.is_request() {
                                "request"
                            } else {
                                "send"
                            };
                            // reqwest::Error's Display embeds the request URL,
                            // which includes our random per-call `nonce` query
                            // param. Stripping it keeps `failure_reasons` low-
                            // cardinality for DD ingestion; the full error
                            // remains at DEBUG above.
                            return Err(format!("{category}: {}", e.without_url()));
                        }
                        Err(_) => {
                            debug!(
                                model = %model,
                                index = backend_index,
                                algo = %algo,
                                elapsed_ms,
                                "Discovery call timed out"
                            );
                            return Err(format!("timeout: {elapsed_ms}ms"));
                        }
                    };

                    if !resp.status().is_success() {
                        let status = resp.status().as_u16();
                        debug!(
                            model = %model,
                            index = backend_index,
                            algo = %algo,
                            status = status,
                            elapsed_ms,
                            "Discovery call returned non-2xx"
                        );
                        return Err(format!("status: {status}"));
                    }
                    let report: serde_json::Map<String, serde_json::Value> = match resp.json().await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            debug!(
                                model = %model,
                                index = backend_index,
                                algo = %algo,
                                error = %e,
                                "Discovery call returned malformed JSON"
                            );
                            return Err(format!("malformed_json: {}", e.without_url()));
                        }
                    };
                    debug!(
                        model = %model,
                        index = backend_index,
                        algo = %algo,
                        elapsed_ms,
                        "Discovery call succeeded"
                    );
                    Ok((report, nonce, algo))
                }
            })
            .collect::<Vec<_>>();

        let results = futures::future::join_all(futures).await;

        let mut successful_calls = 0usize;
        let mut failed_calls = 0usize;
        let mut pubkeys_by_algo: HashMap<String, String> = HashMap::new();
        let mut verified_this_round: HashSet<String> = HashSet::new();
        let mut observed_fingerprints: Vec<String> = Vec::new();
        let mut verify_failures = 0usize;

        for r in results {
            let (report, nonce, algo) = match r {
                Ok(t) => t,
                Err(reason) => {
                    failed_calls += 1;
                    failure_reasons.push(reason);
                    continue;
                }
            };
            successful_calls += 1;

            match verifier.verify_attestation_report(&report, &nonce).await {
                Ok(verified) => {
                    if let Some(ref vfp) = verified.tls_cert_fingerprint {
                        observed_fingerprints.push(vfp.clone());
                        verified_this_round.insert(vfp.clone());
                    }
                    // Pubkey is trustworthy once the report is verified. Pubkeys
                    // are derived from the TEE compose hash so they match
                    // across all backends of the same model+algo; first-write
                    // wins, later responses for the same algo are ignored.
                    if let Some(pk) = report.get("signing_public_key").and_then(|v| v.as_str()) {
                        pubkeys_by_algo
                            .entry(algo.clone())
                            .or_insert_with(|| pk.to_string());
                    }
                }
                Err(e) => {
                    warn!(
                        model = %model_name,
                        url = %url,
                        algo = %algo,
                        error = %e,
                        "Attestation verification failed for discovered backend"
                    );
                    failure_reasons.push(format!("verify: {e}"));
                    verify_failures += 1;
                }
            }
        }

        let update = apply_pin_update(
            &fingerprint_state,
            &verified_this_round,
            backend_count,
            failed_calls,
            verify_failures,
        );
        for fp in &update.newly_pinned {
            info!(
                model = %model_name,
                url = %url,
                fingerprint = %fp,
                "Pinned new TLS SPKI fingerprint from attestation discovery"
            );
        }
        for fp in &update.evicted {
            info!(
                model = %model_name,
                url = %url,
                fingerprint = %fp,
                "Evicted TLS SPKI fingerprint — backend no longer in healthy set"
            );
        }
        let new_fingerprints = update.newly_pinned.len();
        let total_pinned = update.total_pinned;
        let replaced_state = update.replaced;

        DiscoveryOutcome {
            backend_count,
            successful_calls,
            failed_calls,
            new_fingerprints,
            total_pinned,
            pubkeys_by_algo,
            observed_fingerprints,
            failure_reasons,
            verify_failures,
            replaced_state,
        }
    }

    async fn get_providers_for_model(
        &self,
        model_id: &str,
    ) -> Option<Vec<Arc<InferenceProviderTrait>>> {
        let mappings = self.provider_mappings.read().await;
        mappings.model_to_providers.get(model_id).cloned()
    }

    /// Store a mapping of chat_id to provider
    async fn store_chat_id_mapping(
        &self,
        chat_id: String,
        provider: Arc<dyn InferenceProvider + Send + Sync>,
    ) {
        let mut mapping = self.chat_id_mapping.write().await;
        mapping.insert(chat_id.clone(), provider);
        tracing::debug!("Stored chat_id mapping: {}", chat_id);
    }

    /// Lookup provider by chat_id
    pub async fn get_provider_by_chat_id(
        &self,
        chat_id: &str,
    ) -> Option<Arc<dyn InferenceProvider + Send + Sync>> {
        let mapping = self.chat_id_mapping.read().await;
        mapping.get(chat_id).cloned()
    }

    /// Get providers with load balancing support
    ///
    /// This function handles provider selection based on model_id and optional model_pub_key:
    /// - Gets providers by model_id first
    /// - If model_pub_key is provided: Filters providers by public key
    /// - Applies round-robin load balancing
    ///
    /// Returns providers with the round-robin selected one first, followed by others for fallback.
    async fn get_providers_with_fallback(
        &self,
        model_id: &str,
        model_pub_key: Option<&str>,
    ) -> Option<Vec<Arc<InferenceProviderTrait>>> {
        let mappings = self.provider_mappings.read().await;

        // Get providers by model_id first
        let model_providers = mappings.model_to_providers.get(model_id)?.clone();

        // Filter by model_pub_key if provided
        let providers = if let Some(pub_key) = model_pub_key {
            // Use the existing 'mappings' lock instead of acquiring it again
            let pub_key_providers = mappings.pubkey_to_providers.get(pub_key)?.clone();

            // Find intersection: providers that are in both lists
            let filtered: Vec<Arc<InferenceProviderTrait>> = model_providers
                .iter()
                .filter(|model_provider| {
                    pub_key_providers
                        .iter()
                        .any(|pub_provider| Arc::ptr_eq(model_provider, pub_provider))
                })
                .cloned()
                .collect();

            if filtered.is_empty() {
                return None;
            }

            filtered
        } else {
            model_providers
        };

        // Trust-tier safety: a model that can be served by an attested provider
        // (NEAR's own fleet `Near`, or an attested third party like Chutes
        // `Attested3p`) is a *verifiable* model and must NEVER fall back to a
        // non-attested (plaintext) provider — that would silently break the
        // verifiable promise. So whenever any attested provider is present, drop
        // every `NonAttested` one: a verifiable request then fails closed (only
        // attested providers are ever tried) instead of downgrading to plaintext
        // when the attested backends are unhealthy. Pure non-attested models (no
        // attested provider at all, e.g. OpenAI/Anthropic) are unaffected.
        let providers = if providers.iter().any(|p| p.tier().is_attested()) {
            providers
                .into_iter()
                .filter(|p| p.tier().is_attested())
                .collect::<Vec<_>>()
        } else {
            providers
        };

        if providers.is_empty() {
            return None;
        }

        if providers.len() == 1 {
            return Some(providers);
        }

        // Order providers by (health, trust tier), then round-robin WITHIN the
        // leading group. The leading group is the healthy providers of the lowest
        // tier — a NEAR-served model's own attested fleet; requests rotate evenly
        // among them and only fall through to the next tier (an attested third
        // party like Chutes) or to demoted providers when the leading group can't
        // fulfill the request. Rotating within the group (rather than over the full
        // list before sorting) keeps same-tier load balancing even.
        //   tier rank: Near (0) < Attested3p (1) < NonAttested (2)
        const MAX_CONSECUTIVE_FAILURES: u32 = 10;
        fn tier_rank(p: &Arc<InferenceProviderTrait>) -> u8 {
            match p.tier() {
                inference_providers::ProviderTier::Near => 0,
                inference_providers::ProviderTier::Attested3p => 1,
                inference_providers::ProviderTier::NonAttested => 2,
            }
        }
        // (demoted, tier_rank): healthy-before-demoted, NEAR-before-Chutes-before-plaintext.
        let (mut ordered, group_len) = {
            let counts = self
                .provider_failure_counts
                .read()
                .unwrap_or_else(|e| e.into_inner());
            let key_of = |p: &Arc<InferenceProviderTrait>| -> (u8, u8) {
                let failures = counts
                    .get(&(Arc::as_ptr(p) as *const () as usize))
                    .copied()
                    .unwrap_or(0);
                let demoted = u8::from(failures >= MAX_CONSECUTIVE_FAILURES);
                (demoted, tier_rank(p))
            };
            let mut ordered = providers;
            ordered.sort_by_key(&key_of); // stable
            let head_key = key_of(&ordered[0]);
            let group_len = ordered.iter().take_while(|p| key_of(p) == head_key).count();
            (ordered, group_len)
        };

        // Round-robin rotate the leading group so its members share load evenly.
        if group_len > 1 {
            let index_key = if let Some(pub_key) = model_pub_key {
                format!("pubkey:{}", pub_key)
            } else {
                format!("id:{}", model_id)
            };
            let mut indices = self
                .load_balancer_index
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let index = indices.entry(index_key).or_insert(0);
            let rot = *index % group_len;
            *index = (*index + 1) % group_len;
            drop(indices);
            ordered[..group_len].rotate_left(rot);
        }

        tracing::debug!(
            providers_count = ordered.len(),
            leading_group = group_len,
            "Prepared providers for fallback (tier-ordered, round-robin within leading tier)"
        );

        Some(ordered)
    }

    /// Sanitize a CompletionError by preserving its variant structure while sanitizing messages
    fn sanitize_completion_error(error: CompletionError, model_id: &str) -> CompletionError {
        // Helper to sanitize message and format with model_id context
        let sanitize_and_format = |msg: &str| -> String {
            let sanitized = Self::sanitize_error_message(msg);
            format!("Provider failed for model '{}': {}", model_id, sanitized)
        };

        match error {
            CompletionError::HttpError {
                status_code,
                message,
                is_external,
            } => {
                // For HttpError, sanitize the message and include model_id context
                // Preserve status_code and is_external for proper error mapping
                CompletionError::HttpError {
                    status_code,
                    message: sanitize_and_format(&message),
                    is_external,
                }
            }
            CompletionError::CompletionError(msg) => {
                CompletionError::CompletionError(sanitize_and_format(&msg))
            }
            CompletionError::InvalidResponse(msg) => {
                CompletionError::InvalidResponse(sanitize_and_format(&msg))
            }
            CompletionError::Unknown(msg) => CompletionError::Unknown(sanitize_and_format(&msg)),
            CompletionError::ClientMediaError(msg) => {
                CompletionError::ClientMediaError(sanitize_and_format(&msg))
            }
            CompletionError::NoPubKeyProvider(msg) => {
                CompletionError::NoPubKeyProvider(sanitize_and_format(&msg))
            }
            // Timeout carries no caller-controlled string, so there's nothing to
            // sanitize. Keep the structured fields intact so the route handler can
            // surface a precise message.
            CompletionError::Timeout {
                operation,
                timeout_seconds,
            } => CompletionError::Timeout {
                operation,
                timeout_seconds,
            },
        }
    }

    /// Stable label for a CompletionError variant, for log indexing.
    /// Safe to log: contains no caller-controlled content.
    fn classify_error_kind(error: &CompletionError) -> &'static str {
        match error {
            CompletionError::CompletionError(_) => "completion_error",
            CompletionError::HttpError { status_code, .. } => match status_code {
                500..=599 => "http_5xx",
                429 => "http_429",
                408 => "http_408",
                400..=499 => "http_4xx",
                _ => "http_other",
            },
            CompletionError::InvalidResponse(_) => "invalid_response",
            CompletionError::Unknown(_) => "unknown",
            CompletionError::ClientMediaError(_) => "client_media_error",
            CompletionError::NoPubKeyProvider(_) => "no_pubkey_provider",
            CompletionError::Timeout { .. } => "timeout",
        }
    }

    /// Inference engines (vLLM, SGLang) return HTTP 500 when they fail to
    /// fetch or decode a multimodal media URL supplied by the client. The
    /// upstream status is 5xx but the *cause* is a permanent client-input
    /// error — retrying the same payload re-runs the same fetch and produces
    /// the same failure. Treat these as non-retryable so one broken URL
    /// from a client doesn't get amplified into 4x backend work.
    fn is_client_media_fetch_error(message: &str) -> bool {
        // ASCII-only lowercase: the markers are all ASCII and this path can
        // run at high volume during a malformed-media incident.
        let lower = message.to_ascii_lowercase();
        // Pure decode-side failures: the engine fetched the bytes but could not
        // parse them. There is NO fetch HTTP status involved — these are
        // unconditionally permanent client-input faults (a corrupt/unsupported
        // payload re-decodes identically). NOTE: `loading image/video data` is
        // deliberately NOT here. It is the SGLang/vLLM *prefix* for BOTH a decode
        // failure (`... cannot identify image file`, caught here) AND a fetch
        // failure (`... NNN Client Error`, status-gated below). Treating it as
        // decode-only would mis-map a transient `loading IMAGE data ... 503
        // Client Error` to a client 400 — the exact regression the status gate
        // exists to prevent (PR #721 review).
        if lower.contains("cannot identify image file")
            || lower.contains("failed to open input buffer")
        {
            return true;
        }
        // Fetch-side failures: the engine reached out to the client-supplied URL
        // and the *remote host* answered. Only an explicit upstream 4xx is a
        // permanent client-input fault (Wikimedia's 400 for a disallowed
        // User-Agent, a 403, a 404 for a stale URL — retrying the identical URL
        // re-triggers the same rejection). A 5xx (or an indeterminate status)
        // from the remote host is a transient backend problem and MUST remain
        // retryable, so we gate every fetch-side marker on a determinable 4xx.
        // See cloud-api#606 (positive 4xx) and PR #721 review (5xx must retry).
        let has_fetch_marker = lower.contains("failed to fetch image")
            || lower.contains("failed to fetch video")
            || lower.contains("error fetching image")
            || lower.contains("error fetching video")
            || lower.contains("failed to load image")
            || lower.contains("failed to load video")
            || lower.contains("clientresponseerror")
            || lower.contains("client error:")
            // aiohttp wrapper format observed when the inference engine collapses
            // a client-fetch status into a 500: `HTTP error 500: NNN, message=...`.
            || lower.contains("http error 500:")
            // SGLang/vLLM media-load prefix that carries a fetch status in its
            // fetch-failure form (`loading IMAGE data ... NNN Client Error`); the
            // decode form (`... cannot identify image file`) has no status and is
            // already caught above, so status-gating these here is safe.
            || lower.contains("loading image data")
            || lower.contains("loading video data");
        has_fetch_marker
            && Self::extract_fetch_status(&lower).is_some_and(|s| (400..500).contains(&s))
    }

    /// Extract the *upstream fetch* HTTP status embedded in an inference-engine
    /// error message, across the phrasings vLLM/SGLang/aiohttp produce:
    ///
    /// - aiohttp wrapper: `HTTP error 500: 503, message='...', url='http...'`
    /// - aiohttp exception: `ClientResponseError, status=503, message='...'`
    /// - aiohttp `raise_for_status()` str: `400, message='Bad Request', url='...'`
    /// - requests/urllib: `503 Client Error: ... for url: http...`
    ///
    /// Returns `None` when no status is determinable (then the caller keeps the
    /// error retryable). Input is expected ASCII-lowercased.
    fn extract_fetch_status(lower: &str) -> Option<u16> {
        // Each pattern captures the *upstream fetch* status from one specific
        // phrasing. We deliberately do NOT match a bare 3-digit number: the
        // aiohttp wrapper's outer envelope is always "http error 500:", and we
        // must capture the inner status (e.g. the `400` in `HTTP error 500:
        // 400, ...`), never the outer 500. Pattern 4 (`NNN, message=`) is
        // anchored to the literal `, message=` that immediately follows the
        // status in aiohttp's `ClientResponseError.__str__`; the wrapper's outer
        // `500` is followed by `: `, not `, message=`, so it can never match it.
        static FETCH_STATUS: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        let re = FETCH_STATUS.get_or_init(|| {
            Regex::new(
                // 1. aiohttp wrapper:            `http error 500: 400, message=`
                // 2. aiohttp exception:          `clientresponseerror, status=400`
                // 3. requests/urllib:            `400 client error: ... for url`
                // 4. aiohttp raise_for_status(): `400, message='bad request', url=`
                r"http error 500:\s*(\d{3})\b|status=(\d{3})\b|\b(\d{3}) client error:|\b(\d{3}), message=",
            )
            .expect("static regex compiles")
        });
        for caps in re.captures_iter(lower) {
            let code = caps
                .get(1)
                .or_else(|| caps.get(2))
                .or_else(|| caps.get(3))
                .or_else(|| caps.get(4))
                .and_then(|m| m.as_str().parse::<u16>().ok());
            if let Some(code) = code {
                if (400..600).contains(&code) {
                    return Some(code);
                }
            }
        }
        None
    }

    /// A provider's "this model isn't served here" response — OpenAI-compatible
    /// engines (vLLM/SGLang) answer an unknown model with a 404/400 like
    /// "The model `X` does not exist." Unlike a genuine client 4xx (bad params),
    /// this is NOT the request's fault: a *different* provider for the same
    /// canonical id (e.g. a Chutes fallback) may serve the model. So the inner
    /// loop falls through to the next provider instead of fast-returning, while
    /// staying non-retryable for the round (re-hitting the same provider would
    /// just repeat the answer, and it's not a backend-health signal).
    ///
    /// Fixes nearai/cloud-api#797: a NEAR backend that didn't recognize a renamed
    /// canonical id (`z-ai/glm-5.1`) 4xx'd and the request never reached the
    /// configured Chutes fallback.
    fn is_model_not_found_error(status_code: u16, message: &str) -> bool {
        if status_code != 400 && status_code != 404 {
            return false;
        }
        let m = message.to_ascii_lowercase();
        m.contains("model_not_found")
            || (m.contains("model") && (m.contains("does not exist") || m.contains("not found")))
    }

    /// Single source of truth for the retry decision: the inner retry loop
    /// gates on `starts_with("retryable_")`, and the terminal error log emits
    /// the label directly so the rationale is visible in production logs.
    fn classify_retry_decision(error: &CompletionError) -> &'static str {
        match error {
            CompletionError::CompletionError(msg) => {
                let lower = msg.to_lowercase();
                let is_inference_timeout = (lower.contains("operation timed out")
                    || lower.contains("timed out after"))
                    && !lower.contains("connect");
                if is_inference_timeout {
                    "non_retryable_inference_timeout"
                } else if lower.contains("connection")
                    || lower.contains("connect")
                    || lower.contains("reset")
                    || lower.contains("broken pipe")
                    || lower.contains("decoding response body")
                    || lower.contains("body error")
                {
                    "retryable_connection_keyword"
                } else {
                    "non_retryable_no_keyword_match"
                }
            }
            CompletionError::HttpError {
                status_code,
                message,
                ..
            } => {
                if *status_code >= 500 {
                    // Engines (vLLM, SGLang) return 500 when they fail to fetch
                    // or decode a client-supplied multimodal media URL. These
                    // are permanent client-input errors — the same payload
                    // can't succeed on retry. Don't amplify load by 4x.
                    if Self::is_client_media_fetch_error(message) {
                        "non_retryable_client_media_error"
                    } else {
                        "retryable_http_5xx"
                    }
                } else if *status_code == 429 {
                    "retryable_http_429"
                } else if *status_code == 408 {
                    // 408 escapes the inner-loop early-return for 4xx so the next
                    // provider is tried, but the outer is_retryable still returns
                    // false (only 5xx/429 retry the round). Distinct label so this
                    // shows up clearly in logs.
                    "non_retryable_http_408"
                } else {
                    "non_retryable_http"
                }
            }
            CompletionError::Timeout { .. } => "non_retryable_explicit_timeout",
            CompletionError::ClientMediaError(_) => "non_retryable_client_media_error",
            CompletionError::NoPubKeyProvider(_) => "non_retryable_no_pubkey_provider",
            CompletionError::InvalidResponse(_) => "non_retryable_invalid_response",
            CompletionError::Unknown(_) => "non_retryable_unknown",
        }
    }

    /// Category label for a privacy-filter error, safe to log. Drops the
    /// upstream response body (which `HttpError.message` carries verbatim)
    /// so a misbehaving filter that echoes its input doesn't route customer
    /// PII into application logs.
    fn privacy_classify_error_category(
        err: &inference_providers::PrivacyClassifyError,
    ) -> &'static str {
        use inference_providers::PrivacyClassifyError as E;
        match err {
            E::HttpError { status_code, .. } => match status_code {
                401 | 403 => "unauthorized",
                429 => "rate_limited",
                503 => "unavailable",
                500..=599 => "server_error",
                400..=499 => "client_error",
                _ => "http_other",
            },
            E::RequestFailed(_) => "request_failed",
        }
    }

    /// Sanitize error message by removing sensitive information like IP addresses, URLs, and internal details
    fn sanitize_error_message(error: &str) -> String {
        let mut sanitized = error.to_string();

        // Remove URLs (http://..., https://...)
        let url_regex = Regex::new(r"https?://[^\s)]+").unwrap();
        sanitized = url_regex
            .replace_all(&sanitized, "[URL_REDACTED]")
            .to_string();

        // Remove standalone IP addresses with ports (e.g., 192.168.0.1:8000)
        let ip_port_regex = Regex::new(r"\b(?:[0-9]{1,3}\.){3}[0-9]{1,3}:\d+\b").unwrap();
        sanitized = ip_port_regex
            .replace_all(&sanitized, "[IP_REDACTED]")
            .to_string();

        // Remove standalone IP addresses (e.g., 192.168.0.1)
        let ip_regex = Regex::new(r"\b(?:[0-9]{1,3}\.){3}[0-9]{1,3}\b").unwrap();
        sanitized = ip_regex
            .replace_all(&sanitized, "[IP_REDACTED]")
            .to_string();

        // Remove specific error details that might leak internal structure
        sanitized = sanitized.replace(
            "error sending request for url",
            "provider connection failed",
        );

        sanitized
    }

    /// Streaming ops: prefer streaming-capable providers. A NEAR-served model's
    /// Chutes fallback may have streaming disabled (`CHUTES_ENABLE_STREAMING` off);
    /// keeping it in the list means a retryable NEAR failure falls through to
    /// Chutes, whose "streaming not enabled" error would overwrite the retryable
    /// NEAR decision — masking the real error AND suppressing NEAR's backoff retry.
    /// So when the operation streams and ANY provider can stream, drop the ones that
    /// can't. If NONE can stream (e.g. a Chutes-only model with streaming off), keep
    /// the list as-is so the provider's own clear error still surfaces.
    /// Operation names (as passed to [`Self::retry_with_fallback`]) that produce a
    /// streamed response and therefore need a streaming-capable provider. Kept as an
    /// EXPLICIT list rather than a `_stream` name-suffix convention so a new
    /// streaming op must be added here deliberately (a silently-misnamed op no longer
    /// skips the filter); `streaming_operations_list_is_explicit` locks it in.
    const STREAMING_OPERATIONS: &'static [&'static str] = &["chat_completion_stream"];

    fn filter_streaming_capable(
        providers: Vec<Arc<InferenceProviderTrait>>,
        operation_name: &str,
    ) -> Vec<Arc<InferenceProviderTrait>> {
        if Self::STREAMING_OPERATIONS.contains(&operation_name)
            && providers.iter().any(|p| p.supports_streaming())
        {
            providers
                .into_iter()
                .filter(|p| p.supports_streaming())
                .collect()
        } else {
            providers
        }
    }

    /// Mirror of [`Self::filter_streaming_capable`] for client-facing E2EE: when a
    /// request carries `x_client_pub_key` (`needs_client_e2ee`) and ANY provider can
    /// serve it, drop the ones that can't (e.g. Chutes, which rejects client-E2EE
    /// non-retryably) — so a retryable NEAR failure doesn't fall through to that hard
    /// rejection, which would mask the NEAR error and suppress its retry. If NONE can
    /// (a Chutes-only model), the list is kept so the clear rejection still surfaces.
    fn filter_client_e2ee_capable(
        providers: Vec<Arc<InferenceProviderTrait>>,
        needs_client_e2ee: bool,
    ) -> Vec<Arc<InferenceProviderTrait>> {
        if needs_client_e2ee && providers.iter().any(|p| p.supports_client_e2ee()) {
            providers
                .into_iter()
                .filter(|p| p.supports_client_e2ee())
                .collect()
        } else {
            providers
        }
    }

    /// Combine the discovered (e.g. NEAR) providers for the models in THIS cycle
    /// with the out-of-band pinned providers (e.g. Chutes), upholding the invariant
    /// that DB discovery can never DROP or overwrite a pinned (attested) provider:
    /// each discovered model keeps its providers PLUS any pinned providers for it,
    /// re-appended and deduped by Arc pointer (`[fresh NEAR..] + [Chutes]`).
    ///
    /// This is **re-append only** and touches ONLY the models present in
    /// `discovered`, so it is safe for a PARTIAL batch (e.g. the admin
    /// `PATCH /v1/admin/models` runtime registration): models not in the batch are
    /// untouched. The stale-prune of pinned ids that have *left* discovery lives in
    /// [`Self::prune_stale_pinned`], which is complete-set-only.
    ///
    /// Pure (no I/O / no locks) so the merge logic is unit-testable.
    fn merge_discovered_and_pinned(
        discovered: HashMap<String, Vec<Arc<InferenceProviderTrait>>>,
        pinned: &HashMap<String, Vec<Arc<InferenceProviderTrait>>>,
    ) -> Vec<(String, Vec<Arc<InferenceProviderTrait>>)> {
        let ptr = |p: &Arc<InferenceProviderTrait>| Arc::as_ptr(p) as *const () as usize;
        let mut out: Vec<(String, Vec<Arc<InferenceProviderTrait>>)> = Vec::new();
        for (name, mut providers) in discovered {
            if let Some(extra) = pinned.get(&name) {
                let present: std::collections::HashSet<usize> =
                    providers.iter().map(&ptr).collect();
                for p in extra {
                    if !present.contains(&ptr(p)) {
                        providers.push(p.clone());
                    }
                }
            }
            out.push((name, providers));
        }
        out
    }

    /// Complete-set-only counterpart to [`Self::merge_discovered_and_pinned`]: given
    /// the COMPLETE set of model names served by this discovery cycle, rebuild any
    /// pinned id ABSENT from it as pinned-only — dropping a now-stale discovered
    /// (e.g. NEAR) backend that has left discovery, so the attested fallback serves
    /// immediately instead of being tried tier-first until ~10 failure-demotions.
    /// Dropped provider Arcs are pruned from `pubkey_to_providers` and the failure
    /// counters so they don't linger as orphaned / `NoPubKeyProvider`-routable entries.
    ///
    /// MUST be called ONLY on the complete-discovery path (the periodic refresh via
    /// [`Self::sync_inference_url_models`]), NEVER on a partial batch like the admin
    /// `PATCH /v1/admin/models` runtime registration — otherwise it would reset
    /// models merely absent from the batch to pinned-only.
    async fn prune_stale_pinned(&self, complete_names: &std::collections::HashSet<String>) {
        let pinned_providers = self
            .pinned_providers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if pinned_providers.is_empty() {
            return;
        }
        let ptr = |p: &Arc<InferenceProviderTrait>| Arc::as_ptr(p) as *const () as usize;
        let mut dropped: std::collections::HashSet<usize> = std::collections::HashSet::new();
        {
            let mut mappings = self.provider_mappings.write().await;
            for (name, pinned) in &pinned_providers {
                if complete_names.contains(name) {
                    continue; // still discovery-served; the merge handled it this cycle
                }
                let pinned_ptrs: std::collections::HashSet<usize> =
                    pinned.iter().map(&ptr).collect();
                let cur_ptrs: std::collections::HashSet<usize> = mappings
                    .model_to_providers
                    .get(name)
                    .map(|cur| cur.iter().map(&ptr).collect())
                    .unwrap_or_default();
                for &p in cur_ptrs.difference(&pinned_ptrs) {
                    dropped.insert(p);
                }
                // Rewrite only when the current list isn't already exactly the pinned
                // set (compare pointer SETS, not lengths — a future partial overwrite
                // could match on length but differ in membership).
                if cur_ptrs != pinned_ptrs {
                    mappings
                        .model_to_providers
                        .insert(name.clone(), pinned.clone());
                }
            }
            // Shared-Arc guard: providers are cached per-URL, so two model names can
            // share one Arc. Don't prune a dropped pointer's pubkey/counter entries
            // if that exact Arc still backs another (still-served) model — that would
            // transiently break the sibling's pubkey-routed selection.
            if !dropped.is_empty() {
                let still_referenced: std::collections::HashSet<usize> = mappings
                    .model_to_providers
                    .values()
                    .flat_map(|ps| ps.iter().map(&ptr))
                    .collect();
                dropped.retain(|p| !still_referenced.contains(p));
            }
            if !dropped.is_empty() {
                mappings.pubkey_to_providers.retain(|_, ps| {
                    ps.retain(|p| !dropped.contains(&ptr(p)));
                    !ps.is_empty()
                });
            }
        }
        if !dropped.is_empty() {
            self.provider_failure_counts
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|k, _| !dropped.contains(k));
        }
    }

    /// Generic retry helper that tries each provider in order with automatic fallback.
    /// Returns both the result and the provider that succeeded (for chat_id mapping).
    /// If model_pub_key is provided, routes to the specific provider by signing public key.
    /// Thin wrapper that requests no extra capability (see [`Self::retry_with_fallback_caps`]).
    async fn retry_with_fallback<T, F, Fut>(
        &self,
        model_id: &str,
        operation_name: &str,
        model_pub_key: Option<&str>,
        provider_fn: F,
    ) -> Result<(T, Arc<InferenceProviderTrait>), CompletionError>
    where
        F: Fn(Arc<InferenceProviderTrait>) -> Fut,
        Fut: std::future::Future<Output = Result<T, CompletionError>>,
    {
        self.retry_with_fallback_caps(model_id, operation_name, model_pub_key, false, provider_fn)
            .await
    }

    /// As [`Self::retry_with_fallback`], but filters the provider list to those that
    /// can serve the request's capabilities before the fallback loop: streaming
    /// (by `operation_name`) and — when `needs_client_e2ee` — client-facing E2EE. A
    /// capability-incapable provider is dropped only when a capable sibling exists,
    /// so it can't mask the primary's failure / suppress retry, while a model whose
    /// only provider lacks the capability still surfaces that provider's clear error.
    async fn retry_with_fallback_caps<T, F, Fut>(
        &self,
        model_id: &str,
        operation_name: &str,
        model_pub_key: Option<&str>,
        needs_client_e2ee: bool,
        provider_fn: F,
    ) -> Result<(T, Arc<InferenceProviderTrait>), CompletionError>
    where
        F: Fn(Arc<InferenceProviderTrait>) -> Fut,
        Fut: std::future::Future<Output = Result<T, CompletionError>>,
    {
        let providers = match self
            .get_providers_with_fallback(model_id, model_pub_key)
            .await
        {
            Some(p) => p,
            None => {
                if let Some(pub_key) = model_pub_key {
                    let (available_pubkeys, model_provider_count) = {
                        let mappings = self.provider_mappings.read().await;
                        let pubkeys: Vec<String> = mappings
                            .pubkey_to_providers
                            .keys()
                            .map(|k| {
                                let prefix: String = k.chars().take(16).collect();
                                format!("{}...({})", prefix, k.len())
                            })
                            .collect();
                        let count = mappings
                            .model_to_providers
                            .get(model_id)
                            .map(|v| v.len())
                            .unwrap_or(0);
                        (pubkeys, count)
                    };
                    let model_pub_key_prefix: String = pub_key.chars().take(16).collect();
                    tracing::warn!(
                        model_id = %model_id,
                        model_pub_key_prefix = %model_pub_key_prefix,
                        model_pub_key_len = pub_key.len(),
                        available_pubkeys = ?available_pubkeys,
                        model_provider_count = model_provider_count,
                        operation = operation_name,
                        "No provider found for model public key"
                    );
                    return Err(CompletionError::NoPubKeyProvider(format!(
                        "No provider found for model {} with public key '{}...'",
                        model_id,
                        pub_key.chars().take(32).collect::<String>()
                    )));
                } else {
                    let mappings = self.provider_mappings.read().await;
                    let available_models: Vec<_> = mappings.model_to_providers.keys().collect();
                    tracing::error!(
                        model_id = %model_id,
                        available_models = ?available_models,
                        operation = operation_name,
                        "Model not found in provider pool"
                    );
                    return Err(CompletionError::CompletionError(format!(
                        "Model '{model_id}' not found in any configured provider"
                    )));
                }
            }
        };

        let providers = Self::filter_streaming_capable(providers, operation_name);
        let providers = Self::filter_client_e2ee_capable(providers, needs_client_e2ee);

        tracing::info!(
            model_id = %model_id,
            providers_count = providers.len(),
            operation = operation_name,
            "Attempting {} with {} provider(s)",
            operation_name,
            providers.len()
        );

        // Exponential backoff retry for transient errors.
        // Most models have only 1 provider (via model-proxy), so provider fallback alone doesn't help.
        //
        // Connection/5xx: 500ms → 1s → 2s → 4s (4 retries)
        // 429 rate limit:   1s  → 2s → 4s → 8s (4 retries)
        const MAX_RETRIES: usize = 3;
        const CONNECTION_INITIAL_DELAY: Duration = Duration::from_millis(500);
        const CONNECTION_MAX_DELAY: Duration = Duration::from_secs(4);
        const RATE_LIMIT_INITIAL_DELAY: Duration = Duration::from_secs(1);
        const RATE_LIMIT_MAX_DELAY: Duration = Duration::from_secs(8);

        // Track the last error (preserving its structure for proper status code mapping)
        let mut last_error: Option<CompletionError> = None;
        // Retry decision computed from the RAW error before sanitization redacts
        // URLs to `[URL_REDACTED]`. Sharing one decision across the retry gate,
        // the failure-counter gate, and the terminal log keeps them consistent
        // and prevents the regex matchers in classify_retry_decision from
        // being defeated by sanitization.
        let mut last_retry_decision: Option<&'static str> = None;
        let mut total_attempts: usize = 0;
        let mut retry_count: usize = 0;
        let started_at = std::time::Instant::now();
        // Snapshot the full model→providers count once. Reading it again at the
        // failure path can race with a concurrent provider refresh, which would
        // give an inconsistent number relative to `providers_tried`.
        let model_provider_count = self
            .provider_mappings
            .read()
            .await
            .model_to_providers
            .get(model_id)
            .map(|v| v.len())
            .unwrap_or(0);

        loop {
            // Try each provider in order until one succeeds
            for (attempt, provider) in providers.iter().enumerate() {
                total_attempts += 1;
                tracing::debug!(
                    model_id = %model_id,
                    attempt = attempt + 1,
                    total_providers = providers.len(),
                    retry = retry_count,
                    operation = operation_name,
                    "Trying provider {} of {} (retry {})",
                    attempt + 1,
                    providers.len(),
                    retry_count
                );

                match provider_fn(provider.clone()).await {
                    Ok(result) => {
                        // Reset failure counter on success
                        {
                            let mut counts = self
                                .provider_failure_counts
                                .write()
                                .unwrap_or_else(|e| e.into_inner());
                            let key = Arc::as_ptr(provider) as *const () as usize;
                            counts.insert(key, 0);
                        }
                        // Visibility into tiered routing: which trust tier actually
                        // served, and whether we fell back off the primary. `attempt`
                        // is the 0-based index into the tier-ordered provider list, so
                        // anything past index 0 means the leading (primary, e.g. NEAR)
                        // tier failed and a fallback (e.g. Chutes) served the request.
                        let tier = provider.tier();
                        let is_fallback = attempt > 0;
                        tracing::info!(
                            model_id = %model_id,
                            attempt = attempt + 1,
                            retry = retry_count,
                            operation = operation_name,
                            provider_tier = tier.as_str(),
                            is_fallback,
                            "Successfully completed request with provider"
                        );
                        // Emit a fallback counter (every served request, tagged) so
                        // dashboards can show Chutes-served traffic and the
                        // NEAR→fallback rate per model. Recorded here — the only point
                        // that knows the serving provider's tier and the attempt index.
                        if let Some(metrics) = self.metrics_service.get() {
                            metrics.record_count(
                                crate::metrics::consts::METRIC_PROVIDER_REQUESTS,
                                1,
                                &[
                                    &format!("model:{model_id}"),
                                    &format!("provider_tier:{}", tier.as_str()),
                                    &format!("fallback:{is_fallback}"),
                                    &format!("operation:{operation_name}"),
                                ],
                            );
                        }
                        return Ok((result, provider.clone()));
                    }
                    Err(e) => {
                        // For HTTP client errors (4xx), don't retry with other providers.
                        // The request itself is invalid (e.g., too many tokens), so retrying won't help.
                        // Exception: 429 (rate limit) and 408 (request timeout) are retryable
                        // as other providers may have capacity or better connectivity.
                        // NOTE: Don't increment the failure counter for non-retryable 4xx —
                        // these indicate invalid requests, not unhealthy providers.
                        if let CompletionError::HttpError {
                            status_code,
                            message,
                            ..
                        } = &e
                        {
                            // A "model not found" 4xx means THIS provider doesn't
                            // serve the model — fall through to the next provider
                            // (e.g. a Chutes fallback under the same canonical id)
                            // instead of fast-returning, just like 408/429.
                            // (nearai/cloud-api#797.) It is NOT marked retryable
                            // below, so the round isn't re-run against this same
                            // provider and the failure counter isn't bumped; if no
                            // other provider serves it, the error still propagates
                            // after the loop.
                            if (400..=499).contains(status_code)
                                && *status_code != 429
                                && *status_code != 408
                                && !Self::is_model_not_found_error(*status_code, message)
                            {
                                tracing::warn!(
                                    model_id = %model_id,
                                    attempt = attempt + 1,
                                    status_code,
                                    error_detail = %e,
                                    operation = operation_name,
                                    "Client error from provider, not retrying"
                                );
                                return Err(Self::sanitize_completion_error(e, model_id));
                            }
                        }

                        // Classify the retry decision on the RAW error (before
                        // sanitize_completion_error redacts URLs). Used for the
                        // failure-counter gate below, the retry gate after this
                        // loop, and the terminal "All providers failed" log.
                        let retry_decision = Self::classify_retry_decision(&e);
                        let is_retryable_error = retry_decision.starts_with("retryable_");

                        // Short-circuit on client-media-fetch failures the same
                        // way as the 4xx fast-return above: the bad client URL
                        // cannot succeed on any provider, so don't try the rest
                        // — and don't let a later provider's retryable 5xx flip
                        // the outer gate back to "retry the whole round," which
                        // would re-hit this same payload on this same provider.
                        if retry_decision == "non_retryable_client_media_error" {
                            tracing::warn!(
                                model_id = %model_id,
                                attempt = attempt + 1,
                                retry_decision,
                                error_detail = %e,
                                operation = operation_name,
                                "Client media-fetch failure, not retrying or trying other providers"
                            );
                            // Carry the decision as a typed variant (classified
                            // here on the RAW body) so the status mapping maps it
                            // to 400 directly, instead of re-deriving it from the
                            // sanitized, URL-redacted message (which would miss
                            // the URL-bearing forms). sanitize redacts the carried
                            // diagnostic text for safe logging.
                            return Err(Self::sanitize_completion_error(
                                CompletionError::ClientMediaError(e.to_string()),
                                model_id,
                            ));
                        }

                        // Increment failure counter only for retryable errors —
                        // backend-health signals (5xx, timeouts, network errors).
                        // Non-retryable client-input causes (e.g. a 5xx whose body
                        // says "loading IMAGE data … cannot identify image file")
                        // would otherwise demote a healthy backend on every broken
                        // client URL.
                        if is_retryable_error {
                            let mut counts = self
                                .provider_failure_counts
                                .write()
                                .unwrap_or_else(|e| e.into_inner());
                            let key = Arc::as_ptr(provider) as *const () as usize;
                            let counter = counts.entry(key).or_insert(0);
                            *counter = counter.saturating_add(1);
                        }

                        // Log the failure for debugging (before sanitization strips details)
                        let error_kind = Self::classify_error_kind(&e);
                        tracing::warn!(
                            model_id = %model_id,
                            attempt = attempt + 1,
                            retry = retry_count,
                            error_kind,
                            retry_decision,
                            error_detail = %e,
                            operation = operation_name,
                            "Provider failed, will try next provider if available"
                        );

                        // Sanitize and preserve the last error with its structure intact.
                        // Carry the raw-error retry decision so downstream gates and the
                        // terminal log don't re-classify the sanitized form.
                        last_error = Some(Self::sanitize_completion_error(e, model_id));
                        last_retry_decision = Some(retry_decision);
                    }
                }
            }

            // Retry on connection failures, server errors (5xx), and rate limits (429).
            // CompletionError::CompletionError can also contain non-transient errors
            // (e.g., JSON parse failures), so check for connection-related keywords.
            //
            // CompletionError::Timeout (per-call timeout fired against our own vLLM
            // backend) is explicitly NOT retryable: the request was sent and the
            // model is presumably still chewing on it. Retrying the same prompt at
            // the same backend will hit the same wall — and 4× a long completion
            // timeout is an expensive way to surface the same answer.
            //
            // Connect-level timeouts ARE retryable, though: they indicate the
            // request hadn't reached the backend yet, so a retry has a real shot
            // at succeeding. reqwest stringifies these as
            // "error sending request: operation timed out (connect)", so we look
            // for "connect" alongside the timeout signature to keep them retryable.
            //
            // The actual classification lives in `classify_retry_decision` (used
            // for both the retry gate and log labels) so the two can't drift.
            // Use the decision computed from the *raw* error in the loop body —
            // sanitize_completion_error has since redacted URLs to
            // [URL_REDACTED], which would defeat the matcher's url='https?://
            // anchor.
            let is_retryable = last_retry_decision
                .map(|d| d.starts_with("retryable_"))
                .unwrap_or(false);

            if !is_retryable || retry_count >= MAX_RETRIES {
                break;
            }
            retry_count += 1;

            let is_rate_limit = matches!(
                &last_error,
                Some(CompletionError::HttpError { status_code, .. }) if *status_code == 429
            );
            let delay = if is_rate_limit {
                let exp = RATE_LIMIT_INITIAL_DELAY.saturating_mul(1 << (retry_count - 1).min(3));
                exp.min(RATE_LIMIT_MAX_DELAY)
            } else {
                let exp = CONNECTION_INITIAL_DELAY.saturating_mul(1 << (retry_count - 1).min(3));
                exp.min(CONNECTION_MAX_DELAY)
            };
            let reason = if is_rate_limit {
                "rate limit (429)"
            } else {
                "transient error"
            };
            tracing::info!(
                model_id = %model_id,
                retry = retry_count,
                delay_ms = delay.as_millis() as u64,
                operation = operation_name,
                "Retrying after {}", reason
            );
            tokio::time::sleep(delay).await;
        }
        // Pull the diagnostic fields once so both branches share them.
        // These reveal *why* we stopped retrying — without them, the error log
        // alone can't tell apart "1 attempt because non-retryable" from
        // "1 attempt because only 1 provider matched the pubkey" from
        // "exhausted MAX_RETRIES on a retryable error".
        let error_kind = last_error
            .as_ref()
            .map(Self::classify_error_kind)
            .unwrap_or("none");
        // Use the decision computed from the raw error in the loop body, not a
        // re-classification of the sanitized last_error (URLs there are
        // [URL_REDACTED] which would defeat the matcher's url-anchored regex).
        let retry_decision = last_retry_decision.unwrap_or("none");
        let elapsed_ms = started_at.elapsed().as_millis();
        if let Some(pub_key) = model_pub_key {
            tracing::error!(
                model_id = %model_id,
                model_pub_key_prefix = %pub_key.chars().take(32).collect::<String>(),
                providers_tried = providers.len(),
                model_provider_count,
                pubkey_filtered = true,
                total_attempts,
                retry_count,
                error_kind,
                retry_decision,
                elapsed_ms,
                operation = operation_name,
                "All providers failed for model with public key"
            );
        } else {
            tracing::error!(
                model_id = %model_id,
                providers_tried = providers.len(),
                model_provider_count,
                pubkey_filtered = false,
                total_attempts,
                retry_count,
                error_kind,
                retry_decision,
                elapsed_ms,
                operation = operation_name,
                "All providers failed for model"
            );
        }

        // Return the last error, preserving its HttpError variant for proper status code mapping
        match last_error {
            Some(CompletionError::HttpError {
                status_code,
                message,
                is_external,
            }) => Err(CompletionError::HttpError {
                status_code,
                message: if providers.len() > 1 {
                    format!(
                        "All {} provider(s) failed for model '{}'. Last error: {}",
                        providers.len(),
                        model_id,
                        message
                    )
                } else {
                    message
                },
                is_external,
            }),
            Some(other_error) => Err(other_error),
            None => Err(CompletionError::CompletionError(format!(
                "No providers available for model '{}'",
                model_id
            ))),
        }
    }

    pub async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, AttestationError> {
        let providers = self
            .get_providers_for_model(&model)
            .await
            .ok_or_else(|| AttestationError::ProviderNotFound(model.clone()))?;

        // Each inference_url points to a proxy that load-balances across CVMs.
        // All CVMs behind the proxy share the same signing key (derived from model
        // name via dstack KMS), so one attestation report is sufficient.
        // Try providers in order and return the first successful response.
        let mut last_error = None;
        for provider in providers {
            match provider
                .get_attestation_report(
                    model.clone(),
                    signing_algo.clone(),
                    nonce.clone(),
                    signing_address.clone(),
                    include_tls_fingerprint,
                )
                .await
            {
                Ok(mut attestation) => {
                    attestation.remove("all_attestations");
                    return Ok(vec![attestation]);
                }
                Err(e) => {
                    tracing::debug!(
                        model = %model,
                        error = %e,
                        "Provider returned error for attestation request, trying next"
                    );
                    last_error = Some(e);
                }
            }
        }

        Err(last_error
            .map(|e| AttestationError::FetchError(e.to_string()))
            .unwrap_or_else(|| AttestationError::ProviderNotFound(model)))
    }

    pub async fn chat_completion_stream(
        &self,
        mut params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        let model_id = params.model.clone();

        // Extract model_pub_key from params.extra for routing
        let model_pub_key_str = params
            .extra
            .remove(encryption_headers::MODEL_PUB_KEY)
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let model_pub_key = model_pub_key_str.as_deref();

        // Client-facing E2EE intent: keep such requests on a capable (NEAR) provider
        // so a retryable NEAR failure doesn't fall through to Chutes' hard rejection.
        let needs_client_e2ee = params
            .extra
            .contains_key(encryption_headers::CLIENT_PUB_KEY);

        let params_for_provider = params.clone();

        tracing::debug!(
            model = %model_id,
            "Starting chat completion stream request"
        );

        let (stream, provider) = self
            .retry_with_fallback_caps(
                &model_id,
                "chat_completion_stream",
                model_pub_key,
                needs_client_e2ee,
                |provider| {
                    let params = params_for_provider.clone();
                    let request_hash = request_hash.clone();
                    async move { provider.chat_completion_stream(params, request_hash).await }
                },
            )
            .await?;

        // Store chat_id mapping for sticky routing by peeking at the first event
        // Must be synchronous to ensure attestation service can find the provider
        let mut peekable = StreamingResultExt::peekable(stream);
        let mut pinned = false;

        // Control events (blank lines, comments — no parsed chunk) may
        // precede the first data chunk. Consume and stash them so the peek
        // below sees the first parsed chunk; they are re-attached in order
        // since their raw bytes are part of the signed stream (issue #701).
        // Bounded by MAX_LEADING_CONTROL_EVENTS so a keepalive-only upstream
        // can't stall stream return or grow the stash unbounded — past the
        // cap we return the stream without pinning a sticky-routing mapping.
        let mut leading_control: Vec<Result<inference_providers::SSEEvent, CompletionError>> =
            Vec::new();
        {
            use futures::StreamExt as _;
            while leading_control.len() < MAX_LEADING_CONTROL_EVENTS
                && matches!(peekable.peek().await, Some(Ok(event)) if event.chunk.is_none())
            {
                if let Some(ev) = peekable.next().await {
                    leading_control.push(ev);
                }
            }
        }

        if let Some(Ok(event)) = peekable.peek().await {
            if let Some(inference_providers::StreamChunk::Chat(chat_chunk)) = &event.chunk {
                let chat_id = chat_chunk.id.clone();
                tracing::info!(
                    chat_id = %chat_id,
                    "Storing chat_id mapping for streaming completion"
                );
                // Pin the dedicated TLS connection so signature fetches
                // reuse the same connection that served this completion.
                provider.pin_chat_connection(&request_hash, &chat_id);
                pinned = true;
                self.store_chat_id_mapping(chat_id, provider.clone()).await;
            }
        }
        if !pinned {
            // Clean up orphaned pending client when peek fails or yields no chat_id
            provider.pin_chat_connection(&request_hash, "");
            provider.unpin_chat_connection("");
        }
        if leading_control.is_empty() {
            Ok(Box::pin(peekable))
        } else {
            use futures::StreamExt as _;
            Ok(Box::pin(
                futures::stream::iter(leading_control).chain(peekable),
            ))
        }
    }

    pub async fn chat_completion(
        &self,
        mut params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<inference_providers::ChatCompletionResponseWithBytes, CompletionError> {
        let model_id = params.model.clone();

        // Extract model_pub_key from params.extra for routing before any cloning.
        // This ensures the key is removed from params.extra so it won't be passed to the provider,
        // and we have a stable reference for routing even if retries occur.
        let model_pub_key_str = params
            .extra
            .remove(encryption_headers::MODEL_PUB_KEY)
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let model_pub_key = model_pub_key_str.as_deref();

        // Client-facing E2EE intent: keep such requests on a capable (NEAR) provider
        // so a retryable NEAR failure doesn't fall through to Chutes' hard rejection.
        let needs_client_e2ee = params
            .extra
            .contains_key(encryption_headers::CLIENT_PUB_KEY);

        tracing::debug!(
            model = %model_id,
            "Starting chat completion request"
        );

        // Clone params after removing model_pub_key to ensure it's not in the cloned version
        let params_for_provider = params.clone();

        let (response, provider) = self
            .retry_with_fallback_caps(
                &model_id,
                "chat_completion",
                model_pub_key,
                needs_client_e2ee,
                |provider| {
                    let params = params_for_provider.clone();
                    let request_hash = request_hash.clone();
                    async move { provider.chat_completion(params, request_hash).await }
                },
            )
            .await?;

        // Store the chat_id mapping SYNCHRONOUSLY before returning
        // This ensures the attestation service can find the provider
        let chat_id = response.response.id.clone();
        tracing::info!(
            chat_id = %chat_id,
            "Storing chat_id mapping for non-streaming completion"
        );
        self.store_chat_id_mapping(chat_id.clone(), provider).await;
        tracing::debug!(
            chat_id = %chat_id,
            "Stored chat_id mapping before returning response"
        );

        Ok(response)
    }

    /// Generate images using the specified model
    pub async fn image_generation(
        &self,
        mut params: ImageGenerationParams,
        request_hash: String,
    ) -> Result<ImageGenerationResponseWithBytes, ImageGenerationError> {
        let model_id = params.model.clone();

        // Extract model_pub_key from params.extra for routing before any cloning.
        // This ensures the key is removed from params.extra so it won't be passed to the provider,
        // and we have a stable reference for routing even if retries occur.
        let model_pub_key_str = params
            .extra
            .remove(encryption_headers::MODEL_PUB_KEY)
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let model_pub_key = model_pub_key_str.as_deref();

        tracing::debug!(
            model = %model_id,
            "Starting image generation request"
        );

        // Clone params once before retry loop to minimize memory operations with large image data.
        // The provider interface requires ImageEditParams by value, so we must clone when calling
        // the provider. We clone once here and reuse across retries rather than cloning on each attempt.
        let cloned_params = params.clone();

        let (response, provider) = self
            .retry_with_fallback(&model_id, "image_generation", model_pub_key, |provider| {
                let params = cloned_params.clone();
                let request_hash = request_hash.clone();
                async move {
                    provider
                        .image_generation(params, request_hash)
                        .await
                        .map_err(|e| CompletionError::CompletionError(e.to_string()))
                }
            })
            .await
            .map_err(|e| ImageGenerationError::GenerationError(e.to_string()))?;

        // Store the chat_id mapping so attestation service can find the provider
        // (same pattern as chat_completion)
        let image_id = response.response.id.clone();
        tracing::info!(
            image_id = %image_id,
            "Storing chat_id mapping for image generation"
        );
        self.store_chat_id_mapping(image_id, provider).await;

        Ok(response)
    }

    pub async fn audio_transcription(
        &self,
        params: AudioTranscriptionParams,
        request_hash: String,
    ) -> Result<AudioTranscriptionResponse, AudioTranscriptionError> {
        let model_id = params.model.clone();
        let file_size_kb = params.file_bytes.len() / 1024;

        tracing::debug!(
            model = %model_id,
            filename = %params.filename,
            file_size_kb = file_size_kb,
            "Starting audio transcription request"
        );

        let (response, _provider) = self
            .retry_with_fallback(&model_id, "audio_transcription", None, |provider| {
                let params = params.clone();
                let request_hash = request_hash.clone();
                async move {
                    provider
                        .audio_transcription(params, request_hash)
                        .await
                        // Classify the provider's failure so the structured HTTP
                        // status survives retry/fallback only where that is
                        // correct. Genuine client-input 4xx (400/413/415/422)
                        // stay structured: the route returns invalid_request_error
                        // and the pool fast-returns without wasting fallback on a
                        // payload that fails everywhere. 5xx/429 stay structured
                        // too, so retry, failover, and the service status mapping
                        // work. Provider-infra 4xx — 401/403 (our creds), 404
                        // (missing/stale route), 408 (timeout) — are NOT client
                        // input: drop them to a generic provider failure so the
                        // request can fall through to other providers and surfaces
                        // as a 5xx server error, not misleading bad-audio guidance.
                        // is_external=false: this is our own vLLM whisper backend.
                        .map_err(|e| match e {
                            AudioTranscriptionError::HttpError {
                                status_code,
                                message,
                            } if is_client_audio_input_status(status_code)
                                || status_code >= 500
                                || status_code == 429 =>
                            {
                                CompletionError::HttpError {
                                    status_code,
                                    message,
                                    is_external: false,
                                }
                            }
                            AudioTranscriptionError::HttpError {
                                status_code,
                                message,
                            } => CompletionError::CompletionError(format!(
                                "HTTP error {status_code}: {message}"
                            )),
                            AudioTranscriptionError::TranscriptionError(msg) => {
                                CompletionError::CompletionError(msg)
                            }
                        })
                }
            })
            .await
            .map_err(|e| match e {
                // Carry the structured status back out for the codes kept
                // structured above (client-input 4xx, 5xx, 429) so the service
                // layer maps them per-code (client 4xx -> invalid_request_error,
                // 5xx -> 502, 503 -> overloaded, 429 -> rate limit). Infra 4xx
                // were already downgraded to a generic provider failure and fall
                // through the `other` arm to a 502 server error.
                CompletionError::HttpError {
                    status_code,
                    message,
                    ..
                } => AudioTranscriptionError::HttpError {
                    status_code,
                    message: Self::sanitize_error_message(&message),
                },
                other => AudioTranscriptionError::TranscriptionError(Self::sanitize_error_message(
                    &other.to_string(),
                )),
            })?;

        tracing::info!(
            model = %model_id,
            duration = ?response.duration,
            "Audio transcription completed successfully"
        );

        Ok(response)
    }

    pub async fn image_edit(
        &self,
        params: ImageEditParams,
        request_hash: String,
    ) -> Result<ImageEditResponseWithBytes, ImageEditError> {
        let model_id = params.model.clone();

        tracing::debug!(
            model = %model_id,
            "Starting image edit request"
        );

        // Wrap params in Arc to enable cheap cloning across retries.
        // Since image data is already Arc<Vec<u8>>, cloning the params struct is now O(1).
        // Each retry clones the Arc pointer (8 bytes) instead of the entire struct.
        let params = Arc::new(params);

        let (response, provider) = self
            .retry_with_fallback(&model_id, "image_edit", None, |provider| {
                let params = params.clone();
                let request_hash = request_hash.clone();
                async move {
                    provider
                        .image_edit(params, request_hash)
                        .await
                        .map_err(|e| CompletionError::CompletionError(e.to_string()))
                }
            })
            .await
            .map_err(|e| ImageEditError::EditError(e.to_string()))?;

        // Store the chat_id mapping so attestation service can find the provider
        // (same pattern as image_generation)
        let image_id = response.response.id.clone();
        tracing::info!(
            image_id = %image_id,
            "Storing chat_id mapping for image edit"
        );
        self.store_chat_id_mapping(image_id, provider).await;

        Ok(response)
    }

    pub async fn rerank(&self, params: RerankParams) -> Result<RerankResponse, RerankError> {
        let model_id = params.model.clone();

        tracing::debug!(
            model = %model_id,
            document_count = params.documents.len(),
            "Starting rerank request"
        );

        let providers = match self.get_providers_with_fallback(&model_id, None).await {
            Some(p) => p,
            None => {
                return Err(RerankError::GenerationError(format!(
                    "Model '{}' not found in provider pool",
                    model_id
                )));
            }
        };

        // Try reranking with each provider (with fallback)
        let mut last_error = None;
        for provider in providers {
            match provider.rerank(params.clone()).await {
                Ok(response) => {
                    tracing::info!(
                        model = %model_id,
                        result_count = response.results.len(),
                        "Rerank completed successfully"
                    );
                    return Ok(response);
                }
                Err(e) => {
                    tracing::warn!(
                        model = %model_id,
                        error = %e,
                        "Rerank failed with provider, trying next"
                    );
                    last_error = Some(e);
                }
            }
        }

        // All providers failed
        let error_msg = last_error
            .map(|e| Self::sanitize_error_message(&e.to_string()))
            .unwrap_or_else(|| "No providers available for reranking".to_string());

        Err(RerankError::GenerationError(error_msg))
    }

    pub async fn embeddings(
        &self,
        model: &str,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, inference_providers::EmbeddingError> {
        tracing::debug!(model = %model, "Starting embeddings request");

        let providers = match self.get_providers_with_fallback(model, None).await {
            Some(p) => p,
            None => {
                return Err(inference_providers::EmbeddingError::RequestFailed(format!(
                    "Model '{}' not found in provider pool",
                    model
                )));
            }
        };

        // Try with each provider (with fallback)
        let mut last_error = None;
        for provider in providers {
            match provider.embeddings_raw(body.clone(), extra.clone()).await {
                Ok(response) => {
                    tracing::info!(model = %model, "Embeddings completed successfully");
                    return Ok(response);
                }
                Err(e) => {
                    tracing::warn!(model = %model, error = %e, "Embeddings failed with provider, trying next");
                    last_error = Some(e);
                }
            }
        }

        // Preserve the HttpError variant so the caller can see the upstream
        // status code and propagate a meaningful response (e.g. 400 for a
        // client-side parameter error). Collapsing to RequestFailed loses the
        // status code and forces every upstream error to surface as 502.
        Err(match last_error {
            Some(inference_providers::EmbeddingError::HttpError {
                status_code,
                message,
            }) => inference_providers::EmbeddingError::HttpError {
                status_code,
                message: Self::sanitize_error_message(&message),
            },
            Some(inference_providers::EmbeddingError::RequestFailed(msg)) => {
                inference_providers::EmbeddingError::RequestFailed(Self::sanitize_error_message(
                    &msg,
                ))
            }
            None => inference_providers::EmbeddingError::RequestFailed(
                "No providers available for embeddings".to_string(),
            ),
        })
    }

    pub async fn privacy_classify(
        &self,
        model: &str,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, inference_providers::PrivacyClassifyError> {
        tracing::debug!(model = %model, "Starting privacy classify request");

        let providers = match self.get_providers_with_fallback(model, None).await {
            Some(p) => p,
            None => {
                return Err(inference_providers::PrivacyClassifyError::RequestFailed(
                    format!("Model '{}' not found in provider pool", model),
                ));
            }
        };

        let mut last_error = None;
        for provider in providers {
            match provider
                .privacy_classify_raw(body.clone(), extra.clone())
                .await
            {
                Ok(response) => {
                    tracing::debug!(model = %model, "Privacy classify completed successfully");
                    return Ok(response);
                }
                Err(e) => {
                    // Privacy-filter error messages may embed the upstream
                    // response body (HttpError carries the verbatim text).
                    // A misbehaving filter that echoes its input would
                    // route customer PII straight to application logs.
                    // Log only the category + status code.
                    tracing::warn!(
                        model = %model,
                        error_category = %Self::privacy_classify_error_category(&e),
                        "Privacy classify failed with provider, trying next"
                    );
                    last_error = Some(e);
                }
            }
        }

        // Final user-facing error: only the status code escapes; no
        // upstream response body. (`sanitize_error_message` would still
        // include the body via Display, so we route around it.)
        let error_msg = last_error
            .as_ref()
            .map(|e| match e {
                inference_providers::PrivacyClassifyError::HttpError { status_code, .. } => {
                    format!("PII detector returned HTTP {status_code}")
                }
                inference_providers::PrivacyClassifyError::RequestFailed(_) => {
                    "PII detector unreachable".to_string()
                }
            })
            .unwrap_or_else(|| "No providers available for privacy classify".to_string());

        Err(inference_providers::PrivacyClassifyError::RequestFailed(
            error_msg,
        ))
    }

    pub async fn score(
        &self,
        params: inference_providers::ScoreParams,
        request_hash: String,
    ) -> Result<inference_providers::ScoreResponse, inference_providers::ScoreError> {
        let model_id = params.model.clone();

        tracing::debug!(model = %model_id, "Starting score request");

        let providers = match self.get_providers_with_fallback(&model_id, None).await {
            Some(p) => p,
            None => {
                return Err(inference_providers::ScoreError::GenerationError(format!(
                    "Model '{}' not found in provider pool",
                    model_id
                )));
            }
        };

        // Try scoring with each provider (with fallback)
        let mut last_error = None;
        for provider in providers {
            match provider.score(params.clone(), request_hash.clone()).await {
                Ok(response) => {
                    tracing::info!(
                        model = %model_id,
                        "Score completed successfully"
                    );
                    return Ok(response);
                }
                Err(e) => {
                    tracing::warn!(
                        model = %model_id,
                        error = %e,
                        "Score failed with provider, trying next"
                    );
                    last_error = Some(e);
                }
            }
        }

        // All providers failed
        let error_msg = last_error
            .map(|e| Self::sanitize_error_message(&e.to_string()))
            .unwrap_or_else(|| "No providers available for scoring".to_string());

        Err(inference_providers::ScoreError::GenerationError(error_msg))
    }

    /// Create an external provider from a model name and provider config JSON.
    /// Returns a tuple of (provider Arc, backend_type string) without inserting it into any map.
    fn create_external_provider(
        &self,
        model_name: &str,
        provider_config: serde_json::Value,
    ) -> Result<(Arc<InferenceProviderTrait>, String), String> {
        // Extract and remove per-model api_key from raw JSON before deserializing into ProviderConfig
        let mut provider_config = provider_config;
        let per_model_api_key = provider_config
            .as_object_mut()
            .and_then(|obj| obj.remove("api_key"))
            .and_then(|v| v.as_str().map(String::from));

        let config: ProviderConfig = serde_json::from_value(provider_config)
            .map_err(|e| format!("Failed to parse provider config: {e}"))?;

        let backend_type = match &config {
            ProviderConfig::OpenAiCompatible { .. } => "openai_compatible".to_string(),
            ProviderConfig::Anthropic { .. } => "anthropic".to_string(),
            ProviderConfig::Gemini { .. } => "gemini".to_string(),
        };

        let api_key = per_model_api_key
            .or_else(|| {
                self.external_configs
                    .get_api_key(&backend_type)
                    .map(|s| s.to_string())
            })
            .ok_or_else(|| {
                format!(
                    "No API key configured for backend type '{}'. \
                     Set the appropriate environment variable (e.g., OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY) \
                     or include 'api_key' in the model's providerConfig",
                    backend_type
                )
            })?;

        let external_config = ExternalProviderConfig {
            model_name: model_name.to_string(),
            provider_config: config,
            api_key,
            timeout_seconds: self.external_configs.timeout_seconds,
        };

        let provider =
            Arc::new(ExternalProvider::new(external_config)) as Arc<InferenceProviderTrait>;
        Ok((provider, backend_type))
    }

    /// Return the set of model names currently registered in provider_mappings.
    pub async fn registered_model_names(&self) -> Vec<String> {
        let mappings = self.provider_mappings.read().await;
        mappings.model_to_providers.keys().cloned().collect()
    }

    /// Sync external providers — just re-loads them into provider_mappings.
    async fn sync_external_providers(&self, models: Vec<(String, serde_json::Value)>) {
        if let Err(e) = self.load_external_providers(models).await {
            warn!(error = %e, "Failed to sync external providers");
        }
    }

    /// Drop the per-provider failure counters for providers replaced this refresh.
    ///
    /// `candidate_ptrs` is the over-approximate "replaced" set built from the old
    /// `model_to_providers` (it can include a pinned provider, which is never in the
    /// URL-cache reused set). `live_ptrs` is the set of pointers still backing some
    /// model after the merge. A counter is pruned only when its provider is a
    /// candidate AND no longer live — so a still-served pinned fallback, or an Arc
    /// shared by another still-served model, keeps its consecutive-failure counter
    /// and can stay demoted. Pure so the invariant is unit-testable without driving
    /// the (network/attestation-bound) refresh path.
    fn prune_replaced_failure_counts(
        counts: &mut HashMap<usize, u32>,
        candidate_ptrs: &std::collections::HashSet<usize>,
        live_ptrs: &std::collections::HashSet<usize>,
    ) {
        counts.retain(|ptr, _| live_ptrs.contains(ptr) || !candidate_ptrs.contains(ptr));
    }

    /// Load models with inference_url as nearai::Providers into provider_mappings.
    ///
    /// For each model, reuses the existing provider (and its warm TLS connections)
    /// if the inference_url hasn't changed since last load. Only creates new providers
    /// for models whose URL changed or that are new.
    ///
    /// # Arguments
    /// * `models` - List of (model_name, inference_url) tuples
    /// Load (or refresh) a set of inference_url models into the provider pool.
    ///
    /// The `partial` flag controls whether this is a merge or a replace:
    ///
    /// - `partial = true` (admin PATCH path): only the provided models are
    ///   upserted — existing state for untouched models is preserved.
    /// - `partial = false` (periodic sync / startup): the URL-provider cache
    ///   and fingerprint-state map are replaced wholesale, pruning entries for
    ///   models that are no longer in the active set.
    pub async fn load_inference_url_models(&self, models: Vec<(String, String)>, partial: bool) {
        if models.is_empty() {
            return;
        }

        let api_key = self.api_key.clone();

        // Check which models can reuse their existing provider (URL unchanged)
        let existing_cache = self.inference_url_providers.read().await;
        let mut reused: Vec<(String, String, Arc<InferenceProviderTrait>)> = Vec::new();
        let mut needs_creation: Vec<(String, String)> = Vec::new();

        for (model_name, url) in &models {
            if let Some(existing) = existing_cache.get(url) {
                reused.push((model_name.clone(), url.clone(), existing.clone()));
            } else {
                needs_creation.push((model_name.clone(), url.clone()));
            }
        }
        drop(existing_cache);

        if !needs_creation.is_empty() {
            info!(
                new = needs_creation.len(),
                reused = reused.len(),
                "Creating new providers for changed/new inference URLs"
            );
        }

        // Phase 1: Create providers for new/changed URLs, probe attestation, and verify.
        // Discovery uses rotation SNI (model-proxy PR #27): fetch the healthy
        // backend count, then fan out one fresh-TCP call per backend index.
        // One cycle = full coverage. The serving provider shares the per-URL
        // `FingerprintState` with discovery, so every pin propagates.
        let verifier = self.attestation_verifier.clone();
        let tls_roots = self.tls_roots.clone();
        let endpoint_futures: Vec<_> = needs_creation
            .iter()
            .map(|(model_name, url)| {
                let model_name = model_name.clone();
                let url = url.clone();
                let api_key = api_key.clone();
                let verifier = verifier.clone();
                let tls_roots = tls_roots.clone();
                async move {
                    let state =
                        Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));

                    let outcome = Self::discover_model(
                        &url,
                        &api_key,
                        &model_name,
                        state.clone(),
                        &tls_roots,
                        &verifier,
                    )
                    .await;

                    // Serving provider with inline backend verification.
                    // Bucket clients are created lazily: on first use, the verifier
                    // connects to a backend, verifies attestation, and pins the
                    // fingerprint. This eliminates failures from undiscovered backends.
                    let backend_verifier = Arc::new(PoolBackendVerifier {
                        api_key: api_key.clone(),
                        model_name: model_name.clone(),
                        tls_roots: tls_roots.clone(),
                        attestation_verifier: verifier.clone(),
                        fingerprint_state: state.clone(),
                    });
                    let serving_provider =
                        Arc::new(nearai::Provider::new_with_verifier(
                            nearai::Config::new(url.clone(), api_key.clone(), None),
                            state.clone(),
                            backend_verifier,
                        ));

                    // Seed the provider's backend_count cache so traffic-time
                    // rotation-SNI fallback knows how many indices to iterate
                    // on the first 5xx — without this, the very first 5xx
                    // before any refresh cycle would skip rotation entirely.
                    serving_provider.set_backend_count(outcome.backend_count);

                    if outcome.total_pinned == 0 {
                        // Fail closed: reject all TLS until a future refresh's
                        // cumulative discovery pins at least one fingerprint.
                        serving_provider.block_connections();
                        warn!(
                            model = %model_name,
                            url = %url,
                            successful_calls = outcome.successful_calls,
                            failed_calls = outcome.failed_calls,
                            verify_failures = outcome.verify_failures,
                            failure_reasons = ?outcome.failure_reasons,
                            "No TLS fingerprints pinned during initial discovery — provider will reject connections until attestation succeeds"
                        );
                    } else {
                        info!(
                            model = %model_name,
                            url = %url,
                            backend_count = outcome.backend_count,
                            successful_calls = outcome.successful_calls,
                            failed_calls = outcome.failed_calls,
                            verify_failures = outcome.verify_failures,
                            new_fingerprints = outcome.new_fingerprints,
                            total_pinned = outcome.total_pinned,
                            replaced_state = outcome.replaced_state,
                            pubkey_algos = ?outcome.pubkeys_by_algo.keys().collect::<Vec<_>>(),
                            observed_fingerprints = ?outcome.observed_fingerprints,
                            failure_reasons = ?outcome.failure_reasons,
                            "Initial attestation discovery complete"
                        );
                        // Pre-warm all bucket clients in the background so the
                        // inline verification cost is paid at startup rather than
                        // on the first request to each bucket.
                        serving_provider.clone().pre_warm();
                    }

                    let serving_trait =
                        serving_provider.clone() as Arc<InferenceProviderTrait>;
                    let pub_keys: Vec<(String, Arc<InferenceProviderTrait>)> = outcome
                        .pubkeys_by_algo
                        .into_values()
                        .map(|pk| (pk, serving_trait.clone()))
                        .collect();

                    (
                        model_name,
                        url,
                        serving_trait,
                        pub_keys,
                        outcome.total_pinned as u32,
                        state,
                    )
                }
            })
            .collect();

        use futures::stream::{self, StreamExt};
        let new_results: Vec<_> = stream::iter(endpoint_futures)
            .buffer_unordered(20)
            .collect()
            .await;

        // Phase 2: Merge reused and new providers, update mappings.
        let mut model_providers: HashMap<String, Vec<Arc<InferenceProviderTrait>>> = HashMap::new();
        let mut pub_key_updates: Vec<(String, Arc<InferenceProviderTrait>)> = Vec::new();
        let mut new_url_cache: HashMap<String, Arc<InferenceProviderTrait>> = HashMap::new();

        // Reused providers (URL unchanged — keep warm TLS connections).
        //
        // For providers whose per-URL `FingerprintState` is tracked (normal prod
        // case), run a small cumulative-discovery pass: N fresh attestation calls
        // with their own reqwest clients, hitting (hopefully) different L4 backends.
        // This accumulates verified TLS fingerprints over time into the shared
        // `FingerprintState`, and harvests signing pubkeys from any responses. A
        // single initial discovery only sees the backend(s) the first few TCP
        // connections happen to hash to; subsequent cycles cover the rest. Once a
        // backend's fingerprint is pinned it stays pinned for the life of the
        // provider (Pinned state only grows).
        //
        // Cumulative discovery also serves as PR #537's self-heal: if a provider
        // has no pubkey mapping (lost during a failed initial fetch), the pubkeys
        // harvested here backfill the mapping. If discovery turns up nothing at
        // all AND the mapping is still missing, evict the URL so it's recreated
        // from a fresh Bootstrap state next cycle.
        //
        // For reused providers WITHOUT tracked fingerprint state (e.g., mock
        // providers injected directly into the cache by tests), fall back to the
        // legacy per-algo refetch path which works against the trait object.
        {
            // Snapshot `pubkey_to_providers` as an immutable set of `(pubkey,
            // provider_ptr)` pairs. Using pairs (not just pointers) is key:
            // a provider can be registered for one algo (ECDSA) but missing
            // the other (Ed25519) — we want to backfill the missing algo's
            // pubkey without skipping just because *some* mapping exists.
            //
            // Kept immutable during the classify loop. A previous revision
            // used `known_ptrs.insert(ptr)` to deduplicate, which mutated the
            // set as we iterated; when two models shared one provider the
            // second iteration wrongly saw "already mapped" because the first
            // iteration had just inserted. Dedup of per-provider work is now
            // tracked separately in `processed_ptrs` below.
            let (mapped_ptrs, existing_pubkey_entries): (HashSet<usize>, HashSet<(String, usize)>) = {
                let mappings = self.provider_mappings.read().await;
                let ptrs = mappings
                    .pubkey_to_providers
                    .values()
                    .flatten()
                    .map(|p| Arc::as_ptr(p) as *const () as usize)
                    .collect();
                let pairs = mappings
                    .pubkey_to_providers
                    .iter()
                    .flat_map(|(pk, providers)| {
                        let pk = pk.clone();
                        providers
                            .iter()
                            .map(move |p| (pk.clone(), Arc::as_ptr(p) as *const () as usize))
                    })
                    .collect();
                (ptrs, pairs)
            };

            // Snapshot tracked fingerprint states — releasing the lock before
            // we await so we don't block other refresh operations.
            let tracked_states: HashMap<String, Arc<std::sync::RwLock<FingerprintState>>> = {
                let map = self.inference_url_fingerprint_states.read().await;
                map.clone()
            };

            // Classify each reused provider and build parallel work queues.
            // Running the per-provider discovery/refetch calls concurrently
            // keeps refresh latency bounded regardless of pool size — with
            // dozens of models, a sequential loop could add minutes per
            // cycle and starve the background refresh task.
            #[derive(Debug)]
            enum ReusedClassification {
                /// Blocked state — short-circuit, no network call, evict.
                EvictBlocked,
                /// Cumulative discovery with the tracked fingerprint_state.
                Discover(Arc<std::sync::RwLock<FingerprintState>>),
                /// Legacy refetch — no tracked fingerprint_state but provider
                /// has no pubkey mapping either. Falls back to the per-algo
                /// refetch path against the trait object (preserves the old
                /// behavior for test-injected MockProviders).
                LegacyRefetch,
                /// No action — provider is healthy; nothing to do this cycle.
                Skip,
            }

            use futures::FutureExt;

            type DiscoveryTask = futures::future::BoxFuture<
                'static,
                (
                    String,
                    String,
                    Arc<InferenceProviderTrait>,
                    DiscoveryOutcome,
                ),
            >;
            type LegacyTask = futures::future::BoxFuture<
                'static,
                (String, String, Vec<(String, Arc<InferenceProviderTrait>)>),
            >;

            let mut urls_to_evict: Vec<String> = Vec::new();
            let mut evicted_models: Vec<String> = Vec::new();
            let mut evicted_provider_ptrs: HashSet<usize> = HashSet::new();
            let mut discovery_tasks: Vec<DiscoveryTask> = Vec::new();
            let mut legacy_tasks: Vec<LegacyTask> = Vec::new();

            // `processed_ptrs` is only used to deduplicate work per provider
            // (the same Arc can back multiple models). Membership lookups
            // against `mapped_ptrs` use the immutable snapshot above.
            let mut processed_ptrs: HashSet<usize> = HashSet::new();

            for (model_name, url, provider) in &reused {
                let ptr = Arc::as_ptr(provider) as *const () as usize;
                let already_processed = !processed_ptrs.insert(ptr);
                if already_processed {
                    // Same provider already classified under another model in
                    // this loop — skip to avoid enqueuing duplicate work.
                    continue;
                }
                let provider_has_any_pubkey_mapping = mapped_ptrs.contains(&ptr);

                let classification = match tracked_states.get(url) {
                    Some(state) => {
                        let is_blocked = matches!(
                            *state.read().unwrap_or_else(|e| e.into_inner()),
                            FingerprintState::Blocked
                        );
                        if is_blocked {
                            ReusedClassification::EvictBlocked
                        } else {
                            // Always run cumulative discovery for tracked
                            // providers: accumulates fingerprints and merges
                            // any missing-algo pubkeys below. The old
                            // "only-if-missing" gate let partial pubkey
                            // mappings (e.g. ECDSA registered, Ed25519
                            // missing) persist forever.
                            ReusedClassification::Discover(state.clone())
                        }
                    }
                    None => {
                        if !provider_has_any_pubkey_mapping {
                            ReusedClassification::LegacyRefetch
                        } else {
                            ReusedClassification::Skip
                        }
                    }
                };

                match classification {
                    ReusedClassification::EvictBlocked => {
                        warn!(
                            model = %model_name,
                            url = %url,
                            "Reused provider is in Blocked state — evicting for fresh recreation"
                        );
                        urls_to_evict.push(url.clone());
                        evicted_models.push(model_name.clone());
                        evicted_provider_ptrs.insert(ptr);
                    }
                    ReusedClassification::Discover(state) => {
                        let model_name = model_name.clone();
                        let url = url.clone();
                        let provider = provider.clone();
                        let api_key = api_key.clone();
                        let verifier = verifier.clone();
                        let tls_roots = tls_roots.clone();
                        // No inter-model stagger: rotation routes each call
                        // to a distinct backend, so per-backend GPU evidence
                        // pressure per cycle is exactly one attestation,
                        // regardless of how many models refresh together.
                        discovery_tasks.push(
                            async move {
                                let outcome = Self::discover_model(
                                    &url,
                                    &api_key,
                                    &model_name,
                                    state,
                                    &tls_roots,
                                    &verifier,
                                )
                                .await;
                                (model_name, url, provider, outcome)
                            }
                            .boxed(),
                        );
                    }
                    ReusedClassification::LegacyRefetch => {
                        let model_name = model_name.clone();
                        let url = url.clone();
                        let provider = provider.clone();
                        legacy_tasks.push(
                            async move {
                                let (keys, _, _) =
                                    Self::fetch_signing_public_keys_for_both_algorithms(
                                        &provider,
                                        &model_name,
                                        &url,
                                    )
                                    .await;
                                (model_name, url, keys)
                            }
                            .boxed(),
                        );
                    }
                    ReusedClassification::Skip => {}
                }
            }

            use futures::stream::{self as fstream, StreamExt};

            // Run both buckets in parallel. Concurrency cap (10) is smaller
            // than the new-provider path's 20 because cumulative discovery
            // isn't critical-path and we don't want to pile on during refresh.
            //
            // Drained manually with `while let Some(x) = stream.next().await`
            // instead of `.collect::<Vec<_>>()` because the collector's HRTB
            // inference trips on tuples containing trait objects like
            // `Arc<dyn InferenceProvider + Send + Sync>` in this context.
            let drive_discovery = async {
                let mut stream = fstream::iter(discovery_tasks).buffer_unordered(10);
                let mut out = Vec::new();
                while let Some(x) = stream.next().await {
                    out.push(x);
                }
                out
            };
            let drive_legacy = async {
                let mut stream = fstream::iter(legacy_tasks).buffer_unordered(10);
                let mut out = Vec::new();
                while let Some(x) = stream.next().await {
                    out.push(x);
                }
                out
            };
            let (discovery_results, legacy_results) = tokio::join!(drive_discovery, drive_legacy);

            for (model_name, url, provider, outcome) in discovery_results {
                if outcome.new_fingerprints > 0 || outcome.replaced_state {
                    info!(
                        model = %model_name,
                        url = %url,
                        backend_count = outcome.backend_count,
                        new_fingerprints = outcome.new_fingerprints,
                        total_pinned = outcome.total_pinned,
                        verify_failures = outcome.verify_failures,
                        replaced_state = outcome.replaced_state,
                        observed_fingerprints = ?outcome.observed_fingerprints,
                        failure_reasons = ?outcome.failure_reasons,
                        "Cumulative discovery expanded pinned backend set"
                    );
                } else {
                    info!(
                        model = %model_name,
                        url = %url,
                        backend_count = outcome.backend_count,
                        successful_calls = outcome.successful_calls,
                        failed_calls = outcome.failed_calls,
                        verify_failures = outcome.verify_failures,
                        total_pinned = outcome.total_pinned,
                        replaced_state = outcome.replaced_state,
                        observed_fingerprints = ?outcome.observed_fingerprints,
                        failure_reasons = ?outcome.failure_reasons,
                        "Cumulative discovery cycle (no new fingerprints)"
                    );
                }

                // Refresh the provider's backend_count cache so the
                // rotation-SNI traffic fallback uses the latest known healthy
                // count. A `count_zero` cycle yields 0 — that's still a
                // useful update because it disables rotation fallback for
                // this provider until the next cycle proves at least one
                // backend healthy again.
                provider.set_backend_count(outcome.backend_count);

                let ptr = Arc::as_ptr(&provider) as *const () as usize;
                let provider_has_any_pubkey_mapping = mapped_ptrs.contains(&ptr);

                // Merge any pubkeys we harvested. Dedup by (pubkey, provider)
                // pair so we don't accumulate duplicates when an algo's
                // mapping already exists but another algo was missing.
                let mut backfilled = 0usize;
                for pk in outcome.pubkeys_by_algo.into_values() {
                    if !existing_pubkey_entries.contains(&(pk.clone(), ptr)) {
                        backfilled += 1;
                        pub_key_updates.push((pk, provider.clone()));
                    }
                }
                if backfilled > 0 {
                    info!(
                        model = %model_name,
                        url = %url,
                        backfilled_pubkeys = backfilled,
                        "Added signing keys for reused provider via cumulative discovery"
                    );
                }

                // Evict only when the provider has no mapping AT ALL and this
                // round produced none either — a complete failure. Providers
                // with partial mappings keep serving while we retry other
                // algos on the next cycle.
                if backfilled == 0 && !provider_has_any_pubkey_mapping && outcome.total_pinned == 0
                {
                    warn!(
                        model = %model_name,
                        url = %url,
                        successful_calls = outcome.successful_calls,
                        failed_calls = outcome.failed_calls,
                        "Reused provider has no pubkey mapping and cumulative discovery found nothing — evicting for fresh recreation"
                    );
                    urls_to_evict.push(url);
                    evicted_models.push(model_name);
                    evicted_provider_ptrs.insert(ptr);
                }
            }

            for (model_name, url, keys) in legacy_results {
                if keys.is_empty() {
                    warn!(
                        model = %model_name,
                        url = %url,
                        "Legacy refetch failed — evicting from cache for fresh recreation"
                    );
                    // Find the provider Arc for this model so we can prune its
                    // pubkey mappings at eviction time.
                    if let Some((_, _, provider)) =
                        reused.iter().find(|(_, u, _)| u == &url).cloned()
                    {
                        evicted_provider_ptrs.insert(Arc::as_ptr(&provider) as *const () as usize);
                    }
                    urls_to_evict.push(url);
                    evicted_models.push(model_name);
                } else {
                    info!(
                        model = %model_name,
                        pub_keys = keys.len(),
                        "Legacy refetch recovered signing keys"
                    );
                    pub_key_updates.extend(keys);
                }
            }

            if !urls_to_evict.is_empty() {
                let evict_set: HashSet<&str> = urls_to_evict.iter().map(|u| u.as_str()).collect();
                reused.retain(|(_, url, _)| !evict_set.contains(url.as_str()));

                {
                    // Never evict a pinned (out-of-band, attested) model — a DB
                    // inference-url model sharing its name (e.g. a Blocked
                    // fingerprint) must not remove the pinned provider, which the
                    // insert guards would then refuse to restore. Mirrors the
                    // guards on the insert paths and remove_stale_providers.
                    let pinned = self
                        .pinned_models
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let mut mappings = self.provider_mappings.write().await;
                    for model in &evicted_models {
                        if pinned.contains(model) {
                            warn!(model = %model, "Skipping eviction of a pinned (attested) provider");
                            continue;
                        }
                        mappings.model_to_providers.remove(model);
                    }
                    // Prune pubkey_to_providers of entries pointing at the
                    // evicted provider Arcs. Otherwise evicted providers stay
                    // alive via their pubkey mapping (keeping their reqwest
                    // clients and, via mapped_ptrs on future refreshes, being
                    // incorrectly classified as "mapped").
                    if !evicted_provider_ptrs.is_empty() {
                        mappings.pubkey_to_providers.retain(|_, providers| {
                            providers.retain(|p| {
                                !evicted_provider_ptrs
                                    .contains(&(Arc::as_ptr(p) as *const () as usize))
                            });
                            !providers.is_empty()
                        });
                    }
                }
                {
                    let mut cache = self.inference_url_providers.write().await;
                    for url in &urls_to_evict {
                        cache.remove(url);
                    }
                }
                {
                    let mut states = self.inference_url_fingerprint_states.write().await;
                    for url in &urls_to_evict {
                        states.remove(url);
                    }
                }
                info!(
                    count = urls_to_evict.len(),
                    "Evicted blocked/dead providers from cache — will recreate next refresh"
                );
            }
        }

        for (model_name, url, provider) in &reused {
            model_providers
                .entry(model_name.clone())
                .or_default()
                .push(provider.clone());
            new_url_cache.insert(url.clone(), provider.clone());
        }

        // Newly created providers
        let mut new_fingerprint_states: HashMap<String, Arc<std::sync::RwLock<FingerprintState>>> =
            HashMap::new();
        for (model_name, url, provider, pub_keys, pinned_count, state) in &new_results {
            info!(
                model = %model_name,
                url = %url,
                pub_keys = pub_keys.len(),
                pinned_fingerprints = pinned_count,
                "Registered inference_url model"
            );
            model_providers
                .entry(model_name.clone())
                .or_default()
                .push(provider.clone());
            pub_key_updates.extend(pub_keys.iter().cloned());
            new_url_cache.insert(url.clone(), provider.clone());
            new_fingerprint_states.insert(url.clone(), state.clone());
        }

        // Atomic update: replace model providers and rebuild pubkey mappings
        {
            let mut mappings = self.provider_mappings.write().await;

            // Collect reused provider ptrs so we can exclude them from pruning.
            // Reused providers keep the same Arc, so their pubkey mappings are still valid.
            let reused_provider_ptrs: std::collections::HashSet<usize> = reused
                .iter()
                .map(|(_, _, p)| Arc::as_ptr(p) as *const () as usize)
                .collect();

            // Collect old provider ptrs for models being replaced, so we can prune pubkeys.
            // Exclude reused providers — they keep their existing pubkey mappings.
            let mut old_provider_ptrs = std::collections::HashSet::new();
            for model_name in model_providers.keys() {
                if let Some(old) = mappings.model_to_providers.get(model_name) {
                    for p in old {
                        let ptr = Arc::as_ptr(p) as *const () as usize;
                        if !reused_provider_ptrs.contains(&ptr) {
                            old_provider_ptrs.insert(ptr);
                        }
                    }
                }
            }

            // Combine the discovered (NEAR) providers with the out-of-band pinned
            // (Chutes) ones so discovery can never DROP or overwrite a pinned
            // provider — see [`Self::merge_discovered_and_pinned`]. This is the exact
            // substitution-prevention the pinning exists for.
            let pinned_providers = self
                .pinned_providers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            for (model_name, providers) in
                Self::merge_discovered_and_pinned(model_providers, &pinned_providers)
            {
                mappings.model_to_providers.insert(model_name, providers);
            }

            if !old_provider_ptrs.is_empty() {
                mappings.pubkey_to_providers.retain(|_, providers| {
                    providers.retain(|p| {
                        !old_provider_ptrs.contains(&(Arc::as_ptr(p) as *const () as usize))
                    });
                    !providers.is_empty()
                });
                // Prune the per-provider failure counters for genuinely-replaced
                // providers. `old_provider_ptrs` OVER-approximates "replaced": a
                // pinned (Chutes) provider is collected into it on every refresh — it
                // isn't in the URL-cache `reused_provider_ptrs` set — yet it is
                // re-merged into `model_to_providers` just above, and one Arc can back
                // several models. So filter against the post-merge live pointers
                // (mirrors `prune_stale_pinned`'s shared-Arc guard): a counter is
                // dropped only when its provider is replaced AND no longer referenced.
                // Without this the prune would reset a still-served pinned fallback's
                // consecutive-failure counter every refresh, so it could never stay
                // demoted (demotion needs >=10 consecutive failures); the map is also
                // keyed by Arc pointer address, so a stale entry could let a future
                // provider at a recycled address inherit a demotion-level count.
                let live_ptrs: std::collections::HashSet<usize> = mappings
                    .model_to_providers
                    .values()
                    .flat_map(|ps| ps.iter().map(|p| Arc::as_ptr(p) as *const () as usize))
                    .collect();
                Self::prune_replaced_failure_counts(
                    &mut self
                        .provider_failure_counts
                        .write()
                        .unwrap_or_else(|e| e.into_inner()),
                    &old_provider_ptrs,
                    &live_ptrs,
                );
            }

            for (key, provider) in pub_key_updates {
                mappings
                    .pubkey_to_providers
                    .entry(key)
                    .or_default()
                    .push(provider);
            }
        }

        // Log pubkey mapping state for debugging E2EE routing issues
        let (pubkey_count, pubkey_summaries) = {
            let mappings = self.provider_mappings.read().await;
            let count = mappings.pubkey_to_providers.len();
            let summaries: Vec<String> = mappings
                .pubkey_to_providers
                .iter()
                .take(10)
                .map(|(k, v)| {
                    let prefix: String = k.chars().take(16).collect();
                    format!("{}...({}chars,{}providers)", prefix, k.len(), v.len())
                })
                .collect();
            (count, summaries)
        };
        info!(
            pubkey_mapping_count = pubkey_count,
            pubkey_summaries = ?pubkey_summaries,
            "pubkey_to_providers state after update"
        );

        // Update the URL→provider cache.
        //
        // Partial mode (admin PATCH): merge new entries into the existing map
        // so that untouched models keep their warm providers and fingerprint
        // state.  Full mode (periodic sync): replace the map entirely so that
        // URLs for models that have been removed are pruned.
        {
            let mut cache = self.inference_url_providers.write().await;
            if partial {
                // Merge: only insert/replace the URLs we just processed.
                for (url, provider) in new_url_cache {
                    cache.insert(url, provider);
                }
            } else {
                *cache = new_url_cache;
            }
        }

        // Rebuild the URL → FingerprintState map so its key set exactly
        // matches the active inference_url set:
        // - Newly-created URLs take the freshly-discovered state.
        // - Reused URLs keep their existing entries (cumulative discovery
        //   mutated the Arc in place).
        // - URLs no longer in the active set are pruned on a full sync —
        //   prevents a slow leak of stale per-URL state as models are added
        //   and removed.  On a partial sync we only insert new states and
        //   leave existing ones untouched.
        {
            let mut states = self.inference_url_fingerprint_states.write().await;
            for (url, state) in new_fingerprint_states {
                states.insert(url, state);
            }
            if !partial {
                let active_urls = self.inference_url_providers.read().await;
                states.retain(|url, _| active_urls.contains_key(url));
            }
        }

        info!(
            total = models.len(),
            reused = reused.len(),
            created = new_results.len(),
            "Loaded inference_url models"
        );
    }

    /// Refresh inference_url models from the database.
    /// Existing entries in provider_mappings are overwritten with new providers.
    async fn sync_inference_url_models(&self, models: Vec<(String, String)>) {
        // Complete-set discovery path (periodic refresh): re-append pinned providers
        // for the discovered models (inside load_inference_url_models's merge), then
        // prune any pinned id that has LEFT discovery to pinned-only. The complete
        // name set is captured here so it works even when `models` is empty (the
        // last NEAR backend disappeared and every pinned id must drop its stale NEAR)
        // — load_inference_url_models early-returns on empty, so the prune is what
        // rebuilds those. Only the refresh calls this; the admin PATCH path calls
        // load_inference_url_models directly (partial batch, no prune).
        let complete_names: std::collections::HashSet<String> =
            models.iter().map(|(name, _)| name.clone()).collect();
        self.load_inference_url_models(models, false).await;
        self.prune_stale_pinned(&complete_names).await;
    }

    /// Remove models from provider_mappings that are not in `valid_model_names`.
    /// Also cleans up load_balancer_index and provider_failure_counts for removed providers.
    async fn remove_stale_providers(&self, valid_model_names: &std::collections::HashSet<String>) {
        // Skip ids that have an actual pinned PROVIDER (e.g. a registered Chutes
        // fallback) — they're served out-of-band and aren't in the DB-backed
        // `valid_model_names`. A *reserved-only* id (in `pinned_models` for the
        // fail-closed external block, but with no provider because Chutes failed to
        // build / the key was missing) is deliberately NOT skipped: if NEAR also
        // drops it, it has no serving provider and must be removed (fail-closed 404)
        // with full cleanup rather than lingering as a dead NEAR mapping.
        let pinned: std::collections::HashSet<String> = self
            .pinned_providers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        let mut mappings = self.provider_mappings.write().await;

        let stale_models: Vec<String> = mappings
            .model_to_providers
            .keys()
            .filter(|k| !valid_model_names.contains(k.as_str()) && !pinned.contains(k.as_str()))
            .cloned()
            .collect();

        if stale_models.is_empty() {
            return;
        }

        // Collect provider ptrs being removed for ancillary cleanup
        let mut removed_ptrs = std::collections::HashSet::new();
        for model_name in &stale_models {
            if let Some(providers) = mappings.model_to_providers.remove(model_name) {
                for p in &providers {
                    removed_ptrs.insert(Arc::as_ptr(p) as *const () as usize);
                }
            }
        }

        // Prune pubkey entries
        mappings.pubkey_to_providers.retain(|_, providers| {
            providers.retain(|p| !removed_ptrs.contains(&(Arc::as_ptr(p) as *const () as usize)));
            !providers.is_empty()
        });

        // Drop mappings lock before touching std::sync locks
        drop(mappings);

        // Clean up load balancer indices and failure counts
        {
            let mut lb = self
                .load_balancer_index
                .write()
                .unwrap_or_else(|e| e.into_inner());
            for model_name in &stale_models {
                lb.remove(model_name);
            }
        }
        self.provider_failure_counts
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| !removed_ptrs.contains(key));

        info!(
            removed = stale_models.len(),
            models = ?stale_models,
            "Removed stale providers not in database"
        );
    }

    /// Start a periodic background task that refreshes all providers from the database.
    ///
    /// Refreshes both inference_url models (VLlm providers) and external providers
    /// (OpenAI, Anthropic, etc.) on each tick. Removes providers for models that
    /// are no longer in the database.
    ///
    /// The first tick is skipped because providers are already loaded at startup.
    /// If `refresh_interval_secs` is 0, this is a no-op.
    pub async fn start_refresh_task(
        self: Arc<Self>,
        source: Arc<dyn ExternalModelsSource>,
        refresh_interval_secs: u64,
    ) {
        if refresh_interval_secs == 0 {
            debug!("Provider refresh disabled (interval is 0)");
            return;
        }

        let handle = tokio::spawn({
            let pool = self.clone();
            async move {
                let mut interval =
                    tokio::time::interval(tokio::time::Duration::from_secs(refresh_interval_secs));
                // Skip the first immediate tick (providers already loaded at startup)
                interval.tick().await;
                loop {
                    interval.tick().await;
                    debug!("Running periodic provider refresh");

                    let mut valid_model_names = std::collections::HashSet::new();

                    // Refresh inference_url models
                    match source.fetch_inference_url_models().await {
                        Ok(models) => {
                            for (name, _) in &models {
                                valid_model_names.insert(name.clone());
                            }
                            pool.sync_inference_url_models(models).await;
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to refresh inference_url models");
                            // On failure, keep all existing inference_url models
                            // (we don't know which are still valid)
                            let mappings = pool.provider_mappings.read().await;
                            valid_model_names.extend(mappings.model_to_providers.keys().cloned());
                            drop(mappings);
                        }
                    }

                    // Refresh external providers
                    match source.fetch_external_models().await {
                        Ok(models) => {
                            for (name, _) in &models {
                                valid_model_names.insert(name.clone());
                            }
                            pool.sync_external_providers(models).await;
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to refresh external providers");
                            // On failure, keep all existing providers
                            let mappings = pool.provider_mappings.read().await;
                            valid_model_names.extend(mappings.model_to_providers.keys().cloned());
                            drop(mappings);
                        }
                    }

                    // Remove providers for models no longer in the database
                    pool.remove_stale_providers(&valid_model_names).await;
                }
            }
        });

        let mut task_handle = self.refresh_task_handle.lock().await;
        *task_handle = Some(handle);
        info!(
            "Provider refresh task started with interval: {} seconds",
            refresh_interval_secs
        );
    }

    /// Shutdown the inference provider pool and cleanup all resources
    pub async fn shutdown(&self) {
        info!("Initiating inference provider pool shutdown");

        // Cancel the refresh task
        let mut task_handle = self.refresh_task_handle.lock().await;
        if let Some(handle) = task_handle.take() {
            handle.abort();
            info!("Refresh task cancelled");
        }
        drop(task_handle);

        // Clear all state
        let model_count = {
            let mut mappings = self.provider_mappings.write().await;
            let count = mappings.model_to_providers.len();
            *mappings = ProviderMappings::new();
            count
        };

        self.load_balancer_index
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.chat_id_mapping.write().await.clear();
        self.provider_failure_counts
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.inference_url_providers.write().await.clear();

        info!(model_count, "Inference provider pool shutdown completed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure mirror of the `discover_model` call-plan: returns `(backend_idx, algo)`
    /// for each of the `max(backend_count, algos.len())` calls. Lets us pin the
    /// invariant without spinning up a real provider + verifier. Drifts only if
    /// the loop in `discover_model` changes; bring this helper in sync if it does.
    fn discover_model_call_plan<'a>(
        backend_count: usize,
        algos: &'a [&'a str],
    ) -> Vec<(usize, &'a str)> {
        let n_calls = backend_count.max(algos.len());
        (0..n_calls)
            .map(|i| (i % backend_count.max(1), algos[i % algos.len()]))
            .collect()
    }

    #[test]
    fn discover_model_single_backend_covers_both_algos() {
        // Regression for nearai/infra#167: pre-fix, `backend_count=1` produced
        // exactly one call with `ALGOS[0]` ("ecdsa"), so Ed25519 was never
        // harvested and E2EE-via-ed25519 failed with HTTP 421 NoPubKeyProvider.
        let algos = ["ecdsa", "ed25519"];
        let plan = discover_model_call_plan(1, &algos);
        assert_eq!(plan.len(), 2, "expected 2 calls to cover both algos");
        // Both calls target the only backend (index 0). The extra iteration
        // wraps via `i % backend_count` so the rotation URL is buildable.
        assert!(plan.iter().all(|(idx, _)| *idx == 0));
        let covered: std::collections::HashSet<&str> = plan.iter().map(|(_, a)| *a).collect();
        for algo in &algos {
            assert!(
                covered.contains(algo),
                "missing algo {algo} in single-backend plan"
            );
        }
    }

    #[test]
    fn discover_model_multi_backend_unchanged() {
        // Multi-backend models were already correct (alternating algos across
        // distinct backends); this test pins that pre-fix behavior.
        let algos = ["ecdsa", "ed25519"];

        // backend_count == ALGOS.len(): one call per backend, both algos.
        let plan = discover_model_call_plan(2, &algos);
        assert_eq!(plan, vec![(0, "ecdsa"), (1, "ed25519")]);

        // backend_count > ALGOS.len(): every backend gets a call, algos alternate.
        let plan = discover_model_call_plan(5, &algos);
        assert_eq!(
            plan,
            vec![
                (0, "ecdsa"),
                (1, "ed25519"),
                (2, "ecdsa"),
                (3, "ed25519"),
                (4, "ecdsa"),
            ]
        );
        // Both algos still covered.
        let covered: std::collections::HashSet<&str> = plan.iter().map(|(_, a)| *a).collect();
        for algo in &algos {
            assert!(covered.contains(algo));
        }
    }

    /// Helper for `apply_pin_update` tests: build a state, run the policy,
    /// return the (PinUpdate, current pinned-set) pair.
    fn run_pin_update(
        initial: Option<&[&str]>,
        observed: &[&str],
        backend_count: usize,
        failed_calls: usize,
        verify_failures: usize,
    ) -> (PinUpdate, HashSet<String>) {
        let state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
        if let Some(initial) = initial {
            let mut guard = state.write().unwrap();
            for fp in initial {
                guard.add_fingerprint((*fp).to_string());
            }
        }
        let verified: HashSet<String> = observed.iter().map(|s| (*s).to_string()).collect();
        let update = apply_pin_update(
            &state,
            &verified,
            backend_count,
            failed_calls,
            verify_failures,
        );
        let after = match &*state.read().unwrap() {
            FingerprintState::Pinned(s) => s.clone(),
            _ => HashSet::new(),
        };
        (update, after)
    }

    #[test]
    fn pin_update_complete_coverage_replaces_set() {
        // Steady state: pin set already has 5 fingerprints, cycle reconfirms
        // all 5. Coverage is complete → replace (no-op replacement).
        let (update, after) = run_pin_update(
            Some(&["a", "b", "c", "d", "e"]),
            &["a", "b", "c", "d", "e"],
            5,
            0,
            0,
        );
        assert!(update.replaced);
        assert_eq!(update.total_pinned, 5);
        assert!(update.newly_pinned.is_empty());
        assert!(update.evicted.is_empty());
        assert_eq!(after.len(), 5);
    }

    #[test]
    fn pin_update_complete_coverage_evicts_dead_backend() {
        // Backend "e" just went unhealthy → count drops to 4, cycle observes
        // 4 distinct fingerprints, full coverage → replace → "e" is gone.
        let (update, after) = run_pin_update(
            Some(&["a", "b", "c", "d", "e"]),
            &["a", "b", "c", "d"],
            4,
            0,
            0,
        );
        assert!(update.replaced);
        assert_eq!(update.total_pinned, 4);
        assert!(update.newly_pinned.is_empty());
        assert_eq!(update.evicted, vec!["e".to_string()]);
        assert!(!after.contains("e"));
        assert!(after.contains("a"));
    }

    #[test]
    fn pin_update_partial_cycle_keeps_existing_fingerprints() {
        // One backend failed mid-cycle (failed_calls=1). We observed 4 of 5
        // healthy. Cannot safely evict the missing one — additive merge.
        let (update, after) = run_pin_update(
            Some(&["a", "b", "c", "d", "e"]),
            &["a", "b", "c", "d"],
            5,
            1,
            0,
        );
        assert!(!update.replaced);
        assert_eq!(update.total_pinned, 5, "no eviction on partial cycle");
        assert!(after.contains("e"));
    }

    #[test]
    fn pin_update_partial_cycle_with_new_fingerprint_grows_additively() {
        // A previously-unknown backend showed up mid-cycle (perhaps the
        // count grew). One other call failed, so coverage is partial — but
        // we still pin the new fingerprint we did verify.
        let (update, after) = run_pin_update(Some(&["a", "b"]), &["a", "b", "f"], 4, 1, 0);
        assert!(!update.replaced);
        assert_eq!(update.newly_pinned, vec!["f".to_string()]);
        assert_eq!(update.total_pinned, 3);
        assert!(after.contains("f"));
    }

    #[test]
    fn pin_update_duplicate_observations_are_not_complete_coverage() {
        // backend_count=5 but the proxy routed two of our calls to the same
        // backend (e.g. registry race during a deploy). We only see 4
        // distinct fingerprints — fall back to additive so we don't drop
        // the missing one.
        let (update, _after) = run_pin_update(
            Some(&["a", "b", "c", "d", "e"]),
            &["a", "a", "b", "c", "d"],
            5,
            0,
            0,
        );
        assert!(!update.replaced);
        assert_eq!(update.total_pinned, 5);
    }

    #[test]
    fn pin_update_verify_failure_blocks_replacement() {
        // Realistic per-cycle shape: 4 fan-outs, 3 verified, 1 verify
        // failure. backend_count == verified.len() can't both hold when
        // verify_failures > 0, so the policy must treat this as partial
        // and keep the stale 'e' from a previous cycle pinned.
        let (update, after) =
            run_pin_update(Some(&["a", "b", "c", "d", "e"]), &["a", "b", "c"], 4, 0, 1);
        assert!(!update.replaced);
        assert_eq!(
            update.total_pinned, 5,
            "stale 'e' is kept; we couldn't verify the cycle was complete"
        );
        assert!(after.contains("e"));
    }

    #[test]
    fn pin_update_zero_backend_count_is_partial() {
        // backend_count=0 means we couldn't get a count or proxy reports
        // no healthy backends — never replace.
        let (update, after) = run_pin_update(Some(&["a", "b"]), &[], 0, 0, 0);
        assert!(!update.replaced);
        assert_eq!(after.len(), 2, "must not evict on zero-count cycle");
    }

    #[test]
    fn pin_update_from_bootstrap_first_full_coverage() {
        // First-ever discovery: state starts Bootstrap, all calls succeed,
        // full coverage. Result is Pinned with exactly the observed set.
        let (update, after) = run_pin_update(None, &["a", "b", "c"], 3, 0, 0);
        assert!(update.replaced);
        assert_eq!(update.newly_pinned.len(), 3);
        assert!(update.evicted.is_empty());
        assert_eq!(after.len(), 3);
    }

    #[test]
    fn test_classify_error_kind() {
        let cases: &[(CompletionError, &str)] = &[
            (
                CompletionError::CompletionError("anything".to_string()),
                "completion_error",
            ),
            (
                CompletionError::HttpError {
                    status_code: 502,
                    message: String::new(),
                    is_external: false,
                },
                "http_5xx",
            ),
            (
                CompletionError::HttpError {
                    status_code: 429,
                    message: String::new(),
                    is_external: false,
                },
                "http_429",
            ),
            (
                CompletionError::HttpError {
                    status_code: 408,
                    message: String::new(),
                    is_external: false,
                },
                "http_408",
            ),
            (
                CompletionError::HttpError {
                    status_code: 404,
                    message: String::new(),
                    is_external: false,
                },
                "http_4xx",
            ),
            (
                CompletionError::HttpError {
                    status_code: 200,
                    message: String::new(),
                    is_external: false,
                },
                "http_other",
            ),
            (
                CompletionError::InvalidResponse(String::new()),
                "invalid_response",
            ),
            (CompletionError::Unknown(String::new()), "unknown"),
            (
                CompletionError::NoPubKeyProvider(String::new()),
                "no_pubkey_provider",
            ),
            (
                CompletionError::Timeout {
                    operation: String::new(),
                    timeout_seconds: 0,
                },
                "timeout",
            ),
        ];
        for (err, want) in cases {
            assert_eq!(
                InferenceProviderPool::classify_error_kind(err),
                *want,
                "wrong kind for {err:?}"
            );
        }
    }

    #[test]
    fn test_classify_retry_decision() {
        // The "Failed to create verified client … Attestation request timed out"
        // string is what we suspect is leaking through as non-retryable on prod;
        // pin its label here so a later refactor can't silently change it.
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::CompletionError(
                "Failed to create verified client after 3 attempts: Attestation request timed out"
                    .to_string()
            )),
            "non_retryable_no_keyword_match",
        );
        // "operation timed out" without "connect" → inference timeout, non-retryable.
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::CompletionError(
                "vLLM: operation timed out after 90s".to_string()
            )),
            "non_retryable_inference_timeout",
        );
        // Same string with "connect" present → retryable.
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::CompletionError(
                "error sending request: operation timed out (connect)".to_string()
            )),
            "retryable_connection_keyword",
        );
        // Plain connection-keyword match.
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::CompletionError(
                "connection reset by peer".to_string()
            )),
            "retryable_connection_keyword",
        );
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::CompletionError(
                "error decoding response body".to_string()
            )),
            "retryable_connection_keyword",
        );
        // HTTP statuses.
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 503,
                message: String::new(),
                is_external: false,
            }),
            "retryable_http_5xx",
        );
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 429,
                message: String::new(),
                is_external: false,
            }),
            "retryable_http_429",
        );
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 408,
                message: String::new(),
                is_external: false,
            }),
            "non_retryable_http_408",
        );
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 404,
                message: String::new(),
                is_external: false,
            }),
            "non_retryable_http",
        );
        // Explicit timeout variant.
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::Timeout {
                operation: "chat".to_string(),
                timeout_seconds: 90,
            }),
            "non_retryable_explicit_timeout",
        );
        // Other variants are explicitly non-retryable (no catch-all so a new
        // CompletionError variant fails to compile until classified here).
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::NoPubKeyProvider(
                String::new()
            )),
            "non_retryable_no_pubkey_provider",
        );
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::InvalidResponse(
                String::new()
            )),
            "non_retryable_invalid_response",
        );
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(
                &CompletionError::Unknown(String::new())
            ),
            "non_retryable_unknown",
        );

        // Upstream 5xx caused by a broken client media URL — must NOT retry
        // (would otherwise amplify load 4x on every broken URL the client sends).
        // Test fixtures use dummy URLs; the matcher only depends on the marker
        // substrings (`loading IMAGE/VIDEO data`, `cannot identify image file`,
        // `Failed to open input buffer`, aiohttp wrapper shape).

        // SGLang gemma4 image-load failure shape:
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 500,
                message: "Internal server error: An exception occurred while loading IMAGE data at index 0: Error while loading data ImageData(url='https://example.test/img.jpg'): 403 Client Error: Forbidden for url: ...".to_string(),
                is_external: false,
            }),
            "non_retryable_client_media_error",
        );
        // vLLM Qwen3.5-122B torchcodec video-load failure shape:
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 500,
                message: "Internal server error: An exception occurred while loading VIDEO data at index 0: Error while loading data https://example.test/vid: SingleStreamDecoder, Failed to open input buffer: Invalid data found when processing input".to_string(),
                is_external: false,
            }),
            "non_retryable_client_media_error",
        );
        // PIL UnidentifiedImageError (client sent base64 mp4 as image_url):
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 500,
                message: "Internal server error: An exception occurred while loading IMAGE data at index 0: Error while loading data ...: cannot identify image file <_io.BytesIO object at 0x7f151d152b10>".to_string(),
                is_external: false,
            }),
            "non_retryable_client_media_error",
        );
        // aiohttp wrapper format: 500 wrapping a 4xx (client fetch failed):
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 500,
                message: "HTTP error 500: 404, message='Not Found', url='https://example.test/img'"
                    .to_string(),
                is_external: false,
            }),
            "non_retryable_client_media_error",
        );
        // aiohttp wrapper format: 500 wrapping a 5xx (transient backend) — MUST
        // remain retryable. The new check requires the wrapped status to be a
        // 4xx; this guards against the regression PierreLeGuen flagged.
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 500,
                message: "HTTP error 500: 503, message='Service Unavailable', url='https://example.test/backend'".to_string(),
                is_external: false,
            }),
            "retryable_http_5xx",
        );
        // 5xx WITHOUT the media-fetch markers — still retryable (real backend hiccup).
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 500,
                message: "engine: KV cache full, retract".to_string(),
                is_external: false,
            }),
            "retryable_http_5xx",
        );
        // Generic 5xx message that happens to contain a url=... but no
        // wrapper-shape and no media markers — still retryable.
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 500,
                message:
                    "internal: failed to dial postgres url='postgres://...' message='conn refused'"
                        .to_string(),
                is_external: false,
            }),
            "retryable_http_5xx",
        );

        // cloud-api#606: Wikimedia (and other hosts with a User-Agent policy)
        // return 400 to the inference engine's default UA. The engine collapses
        // that client-fetch 400 into a 500. This is a permanent client-input
        // fault — the same URL re-fetched with the same UA fails identically —
        // so it must be non-retryable and surface as a 4xx, not a 502.
        // aiohttp-wrapper shape wrapping a 400:
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 500,
                message: "HTTP error 500: 400, message='Bad Request', url='https://upload.wikimedia.org/wikipedia/commons/x.jpg'".to_string(),
                is_external: false,
            }),
            "non_retryable_client_media_error",
        );
        // SGLang "loading IMAGE data ... 400 Client Error: Bad Request" shape:
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 500,
                message: "Internal server error: An exception occurred while loading IMAGE data at index 0: 400 Client Error: Bad Request for url: https://upload.wikimedia.org/wikipedia/commons/x.jpg".to_string(),
                is_external: false,
            }),
            "non_retryable_client_media_error",
        );
        // vLLM MediaConnector fetch-side phrasings (no aiohttp wrapper, no
        // "loading IMAGE data" prefix) — the broadened markers must catch them,
        // but ONLY when they carry an explicit upstream 4xx (PR #721 review). A
        // bare "Failed to fetch image from <url>" with no status is covered by
        // the negative cases below (stays retryable).
        for msg in [
            "Error fetching image: ClientResponseError, status=400, message='Bad Request'",
            "Failed to load image from url: 403 Client Error: Forbidden for url: https://host/x.png",
            "Internal Server Error: Failed to fetch image: 404 Client Error: Not Found for url: https://upload.wikimedia.org/x.jpg",
            // aiohttp `ClientResponseError.__str__` from raise_for_status():
            // `NNN, message='...', url='...'` (no `status=`, no `Client Error:`).
            // PR #721 review 3 (PierreLeGuen) — the Wikimedia default-UA 400 takes
            // this exact shape and must be non-retryable, not a 502.
            "Failed to fetch image: 400, message='Bad Request', url='https://upload.wikimedia.org/wikipedia/commons/x.jpg'",
            "ClientResponseError: 403, message='Forbidden', url='https://host/x.png'",
        ] {
            assert_eq!(
                InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                    status_code: 500,
                    message: msg.to_string(),
                    is_external: false,
                }),
                "non_retryable_client_media_error",
                "expected non-retryable client-media error for: {msg}",
            );
        }

        // PR #721 review (PierreLeGuen): fetch-side markers wrapping a 5xx — or
        // carrying NO determinable status — describe a transient remote-host
        // failure, NOT a permanent client-input fault. They MUST stay retryable
        // so we don't mask a backend hiccup as a 400 client-media error.
        for msg in [
            // aiohttp ClientResponseError carrying a 5xx → retry.
            "Error fetching image: ClientResponseError, status=503, message='Service Unavailable'",
            "Failed to fetch image: ClientResponseError, status=500, message='Internal Server Error'",
            // requests/urllib phrasing carrying a 5xx → retry.
            "Failed to load image from url: 502 Client Error: Bad Gateway for url: https://host/x.png",
            // Fetch marker with no determinable status → retry (can't prove 4xx).
            "Error fetching image: ClientResponseError, message='connection closed'",
            "Failed to fetch image from https://upload.wikimedia.org/x.jpg",
            // aiohttp raise_for_status() str form carrying a 5xx → retry (the
            // `NNN, message=` parser must keep these retryable). PR #721 review 3.
            "Failed to fetch image: 503, message='Service Unavailable', url='https://host/x.jpg'",
            "ClientResponseError: 502, message='Bad Gateway', url='https://host/x.png'",
            // aiohttp wrapper around a 5xx → retry (regression guard, kept here too).
            "Error fetching image: HTTP error 500: 503, message='Service Unavailable', url='https://host/x.jpg'",
            // SGLang "loading IMAGE/VIDEO data" prefix wrapping a transient 5xx →
            // retry. This is the case the 2nd #721 review (PierreLeGuen) flagged:
            // these markers were previously treated as decode-only and would
            // short-circuit a 503 into a client 400.
            "Internal server error: An exception occurred while loading IMAGE data at index 0: 503 Client Error: Service Unavailable for url: https://upload.wikimedia.org/x.jpg",
            "Internal server error: An exception occurred while loading VIDEO data at index 0: 502 Client Error: Bad Gateway for url: https://host/v.mp4",
        ] {
            assert_eq!(
                InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                    status_code: 500,
                    message: msg.to_string(),
                    is_external: false,
                }),
                "retryable_http_5xx",
                "fetch-side error without a 4xx must stay retryable: {msg}",
            );
        }
        // Positive control alongside the negatives: same ClientResponseError
        // phrasing but with an explicit 4xx → non-retryable client-media.
        for msg in [
            "Error fetching image: ClientResponseError, status=403, message='Forbidden'",
            "Error fetching video: ClientResponseError, status=404, message='Not Found'",
        ] {
            assert_eq!(
                InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                    status_code: 500,
                    message: msg.to_string(),
                    is_external: false,
                }),
                "non_retryable_client_media_error",
                "fetch-side error with an explicit 4xx must be client-media: {msg}",
            );
        }

        // Classification is now driven by the embedded upstream status, not by
        // the URL surviving redaction. sanitize_error_message only redacts the
        // URL/IP, so the `404` status survives and the sanitized aiohttp wrapper
        // STILL classifies as a non-retryable client-media error. This is more
        // robust than the prior URL-anchored regex: the verdict no longer depends
        // on classifying before sanitization. (The production flow still
        // classifies on the raw body — see test_client_media_error_verdict_survives_sanitize.)
        let raw_wrapped_4xx =
            "HTTP error 500: 404, message='Not Found', url='https://example.test/img'".to_string();
        let sanitized = InferenceProviderPool::sanitize_error_message(&raw_wrapped_4xx);
        assert!(
            sanitized.contains("[URL_REDACTED]"),
            "sanitize_error_message should redact URLs (got: {sanitized})"
        );
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::HttpError {
                status_code: 500,
                message: sanitized,
                is_external: false,
            }),
            "non_retryable_client_media_error",
            "embedded 404 survives sanitization, so the wrapper still classifies \
             as a non-retryable client-media error (status-driven, not URL-driven)",
        );
    }

    #[test]
    fn test_client_media_error_verdict_survives_sanitize() {
        // The media short-circuit classifies on the RAW body, then carries the
        // verdict as a typed ClientMediaError so the status mapping doesn't have
        // to re-derive it from the sanitized message. The embedded 404 here is a
        // genuine 4xx client-media fault, so it classifies non-retryable on the
        // raw body and is carried as a typed variant regardless of redaction.
        let raw = CompletionError::HttpError {
            status_code: 500,
            message: "HTTP error 500: 404, message='Not Found', url='https://example.test/img.jpg'"
                .to_string(),
            is_external: false,
        };
        // Detected as a client-media error on the raw body.
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&raw),
            "non_retryable_client_media_error",
        );
        // What the short-circuit returns: ClientMediaError(raw), sanitized.
        let carried = InferenceProviderPool::sanitize_completion_error(
            CompletionError::ClientMediaError(raw.to_string()),
            "test-model",
        );
        match carried {
            // Verdict preserved → map_provider_error maps it to 400 directly.
            CompletionError::ClientMediaError(msg) => {
                assert!(
                    msg.contains("[URL_REDACTED]"),
                    "URL must be redacted: {msg}"
                );
                assert!(!msg.contains("https://"), "raw URL must not survive: {msg}");
            }
            other => panic!("expected ClientMediaError to survive sanitize, got {other:?}"),
        }
        // And it still classifies non-retryable as a typed variant.
        assert_eq!(
            InferenceProviderPool::classify_retry_decision(&CompletionError::ClientMediaError(
                "x".to_string()
            )),
            "non_retryable_client_media_error",
        );
    }

    #[test]
    fn test_sanitize_error_message() {
        // Test URL sanitization
        let error = "Failed to perform completion: error sending request for url (http://192.168.0.1:8000/v1/chat/completions)";
        let sanitized = InferenceProviderPool::sanitize_error_message(error);
        assert!(!sanitized.contains("http://"));
        assert!(!sanitized.contains("192.168.0.1"));
        assert!(sanitized.contains("[URL_REDACTED]"));
        assert!(sanitized.contains("provider connection failed"));

        // Test IP with port sanitization
        let error = "Connection failed to 192.168.1.100:8080";
        let sanitized = InferenceProviderPool::sanitize_error_message(error);
        assert!(!sanitized.contains("192.168.1.100"));
        assert!(!sanitized.contains("8080"));
        assert!(sanitized.contains("[IP_REDACTED]"));

        // Test standalone IP sanitization
        let error = "Server at 10.0.0.1 is unreachable";
        let sanitized = InferenceProviderPool::sanitize_error_message(error);
        assert!(!sanitized.contains("10.0.0.1"));
        assert!(sanitized.contains("[IP_REDACTED]"));

        // Test HTTPS URLs
        let error = "Failed to connect to https://api.example.com/v1/endpoint";
        let sanitized = InferenceProviderPool::sanitize_error_message(error);
        assert!(!sanitized.contains("https://api.example.com"));
        assert!(sanitized.contains("[URL_REDACTED]"));

        // Test complex error message (like the one from the screenshot)
        let error = "Failed to perform completion: All 2 provider(s) failed for model 'Qwen/Qwen3-30B-A3B-Instruct-2507' during chat_completion: Provider 1: Failed to perform completion: error sending request for url (http://192.168.0.1:8000/v1/chat/completions): Provider 2: Failed to perform completion: HTTP 401 Unauthorized";
        let sanitized = InferenceProviderPool::sanitize_error_message(error);
        assert!(!sanitized.contains("http://"));
        assert!(!sanitized.contains("192.168.0.1"));
        assert!(!sanitized.contains("8000"));
        assert!(!sanitized.contains("/v1/chat/completions"));
        assert!(sanitized.contains("[URL_REDACTED]"));
        assert!(sanitized.contains("provider connection failed"));

        // Model name should still be present
        assert!(sanitized.contains("Qwen/Qwen3-30B-A3B-Instruct-2507"));

        // HTTP status should still be present (not sensitive)
        assert!(sanitized.contains("401 Unauthorized"));

        // Test that "not found" keywords are preserved for error detection
        // This is important because route handlers check for "not found" to return 404 errors
        let error_not_found =
            "Model 'Qwen/Qwen3-Reranker-0.6B' not found at http://192.168.0.1:8000";
        let sanitized_not_found = InferenceProviderPool::sanitize_error_message(error_not_found);
        assert!(
            sanitized_not_found.contains("not found"),
            "Keywords 'not found' must be preserved for error detection"
        );
        assert!(!sanitized_not_found.contains("http://"));
        assert!(!sanitized_not_found.contains("192.168.0.1"));

        let error_does_not_exist =
            "Model 'gpt-4' does not exist on the server https://api.example.com";
        let sanitized_exists = InferenceProviderPool::sanitize_error_message(error_does_not_exist);
        assert!(
            sanitized_exists.contains("does not exist"),
            "Keywords 'does not exist' must be preserved for error detection"
        );
        assert!(!sanitized_exists.contains("https://api.example.com"));
    }

    #[tokio::test]
    async fn test_streaming_chat_id_mapping_available_immediately() {
        use futures_util::StreamExt;
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());

        let mock_provider = Arc::new(MockProvider::new());
        let model_id = "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string();
        pool.register_provider(model_id.clone(), mock_provider)
            .await;

        let params = inference_providers::ChatCompletionParams {
            model: model_id,
            messages: vec![inference_providers::ChatMessage {
                role: inference_providers::MessageRole::User,
                content: Some(serde_json::Value::String("Hello".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: None,
            stream: Some(true),
            tools: None,
            max_completion_tokens: None,
            n: None,
            frequency_penalty: None,
            presence_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: None,
            seed: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: None,
            store: None,
            stream_options: None,
            modalities: None,
            extra: std::collections::HashMap::new(),
        };

        let mut stream = pool
            .chat_completion_stream(params, "test-request-hash".to_string())
            .await
            .expect("Should create stream");

        let first_event = stream.next().await.unwrap().unwrap();
        let chat_id = match first_event.chunk {
            Some(inference_providers::StreamChunk::Chat(chunk)) => chunk.id,
            _ => panic!("Expected chat chunk"),
        };

        // Mapping must be available immediately (no race with spawn)
        assert!(pool.get_provider_by_chat_id(&chat_id).await.is_some());

        while stream.next().await.is_some() {}
        assert!(pool.get_provider_by_chat_id(&chat_id).await.is_some());
    }

    // ==================== Provider Tests ====================

    #[tokio::test]
    async fn test_load_external_provider_openai() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test-key".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
                ..Default::default()
            },
        );

        let result = pool.load_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
        ]).await;

        assert!(result.is_ok());
        assert!(pool.has_provider("gpt-4").await);
    }

    #[tokio::test]
    async fn test_load_external_provider_anthropic() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: None,
                anthropic_api_key: Some("sk-ant-test".to_string()),
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
                ..Default::default()
            },
        );

        let result = pool.load_external_providers(vec![
            ("claude-3-opus".to_string(), serde_json::json!({"backend": "anthropic", "base_url": "https://api.anthropic.com/v1"})),
        ]).await;

        assert!(result.is_ok());
        assert!(pool.has_provider("claude-3-opus").await);
    }

    #[tokio::test]
    async fn test_load_external_provider_missing_api_key() {
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());

        let result = pool.load_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
        ]).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed to load"));
    }

    #[tokio::test]
    async fn test_load_external_provider_invalid_config() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
                ..Default::default()
            },
        );

        let result = pool.load_external_providers(vec![
            ("test-model".to_string(), serde_json::json!({"backend": "unknown_backend", "base_url": "https://example.com"})),
        ]).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_has_provider_for_registered_model() {
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());

        let mock_provider = Arc::new(MockProvider::new());
        pool.register_provider("vllm-model".to_string(), mock_provider)
            .await;

        assert!(pool.has_provider("vllm-model").await);
        assert!(!pool.has_provider("unknown-model").await);
    }

    #[tokio::test]
    async fn test_load_external_providers_batch() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: Some("sk-ant-test".to_string()),
                gemini_api_key: Some("AIza-test".to_string()),
                timeout_seconds: 60,
                refresh_interval_secs: 0,
                ..Default::default()
            },
        );

        let result = pool.load_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
            ("claude-3".to_string(), serde_json::json!({"backend": "anthropic", "base_url": "https://api.anthropic.com/v1"})),
            ("gemini-pro".to_string(), serde_json::json!({"backend": "gemini", "base_url": "https://generativelanguage.googleapis.com/v1beta"})),
        ]).await;

        assert!(result.is_ok());
        assert!(pool.has_provider("gpt-4").await);
        assert!(pool.has_provider("claude-3").await);
        assert!(pool.has_provider("gemini-pro").await);
    }

    #[tokio::test]
    async fn test_load_external_providers_partial_failure() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
                ..Default::default()
            },
        );

        let result = pool.load_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
            ("claude-3".to_string(), serde_json::json!({"backend": "anthropic", "base_url": "https://api.anthropic.com/v1"})),
        ]).await;

        assert!(result.is_ok());
        assert!(pool.has_provider("gpt-4").await);
        assert!(!pool.has_provider("claude-3").await);
    }

    #[tokio::test]
    async fn test_shutdown_clears_providers() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
                ..Default::default()
            },
        );

        pool.load_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
        ]).await.unwrap();

        assert!(pool.has_provider("gpt-4").await);
        pool.shutdown().await;
        assert!(!pool.has_provider("gpt-4").await);
    }

    #[tokio::test]
    async fn test_unregister_provider() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
                ..Default::default()
            },
        );

        pool.load_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
        ]).await.unwrap();

        assert!(pool.has_provider("gpt-4").await);
        assert!(pool.unregister_provider("gpt-4").await);
        assert!(!pool.has_provider("gpt-4").await);
        assert!(!pool.unregister_provider("gpt-4").await);
    }

    #[tokio::test]
    async fn test_unregister_nonexistent_provider() {
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        assert!(!pool.unregister_provider("nonexistent-model").await);
    }

    #[tokio::test]
    async fn pinned_provider_survives_refresh() {
        use inference_providers::mock::MockProvider;
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());

        // A DB-discovered provider + a config-pinned (e.g. Chutes) provider.
        pool.register_provider("db-model".to_string(), Arc::new(MockProvider::new()))
            .await;
        pool.register_pinned_secondary_provider(
            "chutes-model".to_string(),
            Arc::new(MockProvider::new()),
        )
        .await;

        // A refresh tick's `valid_model_names` comes only from the DB sources, so
        // it knows "db-model" but not the config-pinned "chutes-model".
        let mut valid = std::collections::HashSet::new();
        valid.insert("db-model".to_string());
        pool.remove_stale_providers(&valid).await;

        assert!(
            pool.has_provider("chutes-model").await,
            "pinned provider must survive a refresh that doesn't list it"
        );
        assert!(pool.has_provider("db-model").await);

        // Sanity: a non-pinned model absent from the valid set IS removed.
        pool.register_provider("ephemeral".to_string(), Arc::new(MockProvider::new()))
            .await;
        pool.remove_stale_providers(&valid).await;
        assert!(
            !pool.has_provider("ephemeral").await,
            "non-pinned stale model must be removed"
        );
        assert!(pool.has_provider("chutes-model").await);
    }

    #[tokio::test]
    async fn pinned_provider_not_overwritten_by_discovery() {
        use inference_providers::mock::MockProvider;
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());

        let pinned: Arc<InferenceProviderTrait> = Arc::new(MockProvider::new());
        pool.register_pinned_secondary_provider("chutes-model".to_string(), pinned.clone())
            .await;

        // A DB-discovered external model with the SAME id must NOT replace the
        // attested, per-request-verified pinned provider.
        let _ = pool
            .load_external_providers(vec![(
                "chutes-model".to_string(),
                serde_json::json!({
                    "backend": "openai_compatible",
                    "base_url": "https://example.com/v1",
                    "api_key": "sk-x"
                }),
            )])
            .await;

        let got = pool
            .get_providers_for_model("chutes-model")
            .await
            .expect("model still registered");
        assert_eq!(got.len(), 1);
        assert!(
            Arc::ptr_eq(&got[0], &pinned),
            "DB discovery must not overwrite a pinned (attested) provider"
        );
    }

    #[tokio::test]
    async fn is_pinned_reports_pinned_models_and_guards_admin_unregister() {
        use inference_providers::mock::MockProvider;
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());

        pool.register_pinned_secondary_provider(
            "chutes-model".to_string(),
            Arc::new(MockProvider::new()),
        )
        .await;
        pool.register_provider("db-model".to_string(), Arc::new(MockProvider::new()))
            .await;

        assert!(pool.is_pinned("chutes-model"));
        assert!(!pool.is_pinned("db-model"));
        assert!(!pool.is_pinned("absent-model"));

        // Regression for the admin PATCH unregister guard (admin.rs): a PATCH
        // carrying `provider_type` (e.g. "chutes") would otherwise unregister the
        // provider, and the inference-url/external re-registration below would NOT
        // restore a pinned one — leaving an active catalog row with no provider.
        // The guard skips pinned models, so the pinned provider must survive.
        for model in ["chutes-model", "db-model"] {
            let provider_type_present = true; // PATCH includes provider_type
            if !pool.is_pinned(model) && provider_type_present {
                pool.unregister_provider(model).await;
            }
        }
        assert!(
            pool.has_provider("chutes-model").await,
            "pinned provider must survive an admin PATCH carrying provider_type"
        );
        assert!(
            !pool.has_provider("db-model").await,
            "a non-pinned provider is still unregistered on a provider_type change"
        );
    }

    /// A NEAR-served (verifiable) model orders its own attested fleet first, an
    /// attested third party (Chutes) as fallback, and NEVER includes a
    /// non-attested (plaintext) provider — even when one is registered under the
    /// same canonical id (e.g. a stray OpenRouter/external row).
    #[tokio::test]
    async fn verifiable_model_prefers_near_then_chutes_and_never_plaintext() {
        use inference_providers::mock::MockProvider;
        use inference_providers::ProviderTier;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model = "zai-org/GLM-5.1-FP8".to_string();

        // Insert in deliberately-wrong order to prove tier sorting + the filter.
        pool.register_pinned_secondary_provider(
            model.clone(),
            Arc::new(MockProvider::new().with_tier(ProviderTier::NonAttested)),
        )
        .await;
        pool.register_pinned_secondary_provider(
            model.clone(),
            Arc::new(MockProvider::new().with_tier(ProviderTier::Attested3p)),
        )
        .await;
        pool.register_pinned_secondary_provider(
            model.clone(),
            Arc::new(MockProvider::new().with_tier(ProviderTier::Near)),
        )
        .await;

        let providers = pool
            .get_providers_with_fallback(&model, None)
            .await
            .expect("model has providers");
        let tiers: Vec<ProviderTier> = providers.iter().map(|p| p.tier()).collect();
        assert_eq!(
            tiers,
            vec![ProviderTier::Near, ProviderTier::Attested3p],
            "verifiable model: NEAR primary, Chutes fallback, NON-attested dropped entirely"
        );
    }

    /// A Chutes-only model (NEAR does not serve it) has the single attested
    /// provider as primary.
    #[tokio::test]
    async fn chutes_only_model_serves_as_single_attested_primary() {
        use inference_providers::mock::MockProvider;
        use inference_providers::ProviderTier;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model = "moonshotai/kimi-k2.6".to_string();
        pool.register_pinned_secondary_provider(
            model.clone(),
            Arc::new(MockProvider::new().with_tier(ProviderTier::Attested3p)),
        )
        .await;

        let providers = pool
            .get_providers_with_fallback(&model, None)
            .await
            .expect("model has providers");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].tier(), ProviderTier::Attested3p);
    }

    /// A model with NO attested provider (a plain external model) is unaffected by
    /// the verifiable filter and is still served by its non-attested provider.
    #[tokio::test]
    async fn non_verifiable_model_is_served_by_its_non_attested_provider() {
        use inference_providers::mock::MockProvider;
        use inference_providers::ProviderTier;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model = "openai/gpt-4o".to_string();
        pool.register_provider(
            model.clone(),
            Arc::new(MockProvider::new().with_tier(ProviderTier::NonAttested)),
        )
        .await;

        let providers = pool
            .get_providers_with_fallback(&model, None)
            .await
            .expect("model has providers");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].tier(), ProviderTier::NonAttested);
    }

    /// `register_pinned_secondary_provider` PUSHES (coexists) rather than
    /// overwriting: a DB-discovered NEAR provider and the pinned Chutes fallback
    /// live under one canonical id, NEAR ordered first.
    #[tokio::test]
    async fn pinned_secondary_chutes_coexists_with_db_near_provider() {
        use inference_providers::mock::MockProvider;
        use inference_providers::ProviderTier;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model = "zai-org/GLM-5.1-FP8".to_string();

        // NEAR registered via the DB-discovery path (overwrite semantics)...
        pool.register_provider(
            model.clone(),
            Arc::new(MockProvider::new().with_tier(ProviderTier::Near)),
        )
        .await;
        // ...then Chutes pinned as a secondary (push, not overwrite).
        pool.register_pinned_secondary_provider(
            model.clone(),
            Arc::new(MockProvider::new().with_tier(ProviderTier::Attested3p)),
        )
        .await;

        assert!(pool.is_pinned(&model));
        let providers = pool
            .get_providers_with_fallback(&model, None)
            .await
            .expect("model has providers");
        let tiers: Vec<ProviderTier> = providers.iter().map(|p| p.tier()).collect();
        assert_eq!(
            tiers,
            vec![ProviderTier::Near, ProviderTier::Attested3p],
            "DB NEAR provider and pinned Chutes secondary must coexist, NEAR first"
        );
    }

    /// Streaming-capability filter (review #2 / F1): for a streaming op, a Chutes
    /// fallback with streaming disabled is dropped when a streaming-capable NEAR
    /// sibling exists — so a NEAR failure doesn't fall through to Chutes' hard
    /// "streaming disabled" error. A non-streaming op is untouched; a model whose
    /// only provider can't stream keeps it so the clear error still surfaces.
    #[test]
    fn filter_streaming_capable_prefers_streaming_providers() {
        use inference_providers::mock::MockProvider;
        use inference_providers::ProviderTier;

        let near = Arc::new(MockProvider::new().with_tier(ProviderTier::Near))
            as Arc<InferenceProviderTrait>;
        let chutes_no_stream = Arc::new(
            MockProvider::new()
                .with_tier(ProviderTier::Attested3p)
                .with_streaming_support(false),
        ) as Arc<InferenceProviderTrait>;

        // Streaming op, NEAR can stream + Chutes can't → Chutes dropped.
        let out = InferenceProviderPool::filter_streaming_capable(
            vec![near.clone(), chutes_no_stream.clone()],
            "chat_completion_stream",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tier(), ProviderTier::Near);

        // Non-streaming op → no filtering, both kept.
        let out = InferenceProviderPool::filter_streaming_capable(
            vec![near.clone(), chutes_no_stream.clone()],
            "chat_completion",
        );
        assert_eq!(out.len(), 2);

        // Streaming op, the ONLY provider can't stream → kept (clear error surfaces).
        let out = InferenceProviderPool::filter_streaming_capable(
            vec![chutes_no_stream.clone()],
            "chat_completion_stream",
        );
        assert_eq!(out.len(), 1);
    }

    /// The shared merge is re-append-only and PARTIAL-BATCH SAFE (review round 2,
    /// Pierre's blocking): it re-attaches pinned providers for the DISCOVERED models
    /// and never emits/rebuilds a pinned id absent from this batch — so an admin
    /// `PATCH /v1/admin/models` carrying a partial inference_url batch can't reset
    /// unrelated NEAR+Chutes models to Chutes-only.
    #[test]
    fn merge_discovered_and_pinned_reappends_and_leaves_absent_pinned_untouched() {
        use inference_providers::mock::MockProvider;
        use inference_providers::ProviderTier;
        use std::collections::HashMap;

        let near = Arc::new(MockProvider::new().with_tier(ProviderTier::Near))
            as Arc<InferenceProviderTrait>;
        let chutes = Arc::new(MockProvider::new().with_tier(ProviderTier::Attested3p))
            as Arc<InferenceProviderTrait>;

        let mut pinned: HashMap<String, Vec<Arc<InferenceProviderTrait>>> = HashMap::new();
        pinned.insert("near+chutes".to_string(), vec![chutes.clone()]);
        pinned.insert("other-pinned".to_string(), vec![chutes.clone()]);

        // A PARTIAL batch: only "near+chutes" is discovered this call.
        let mut discovered: HashMap<String, Vec<Arc<InferenceProviderTrait>>> = HashMap::new();
        discovered.insert("near+chutes".to_string(), vec![near.clone()]);

        let merged: HashMap<String, Vec<ProviderTier>> =
            InferenceProviderPool::merge_discovered_and_pinned(discovered, &pinned)
                .into_iter()
                .map(|(k, v)| (k, v.iter().map(|p| p.tier()).collect()))
                .collect();

        // Discovered model: NEAR refreshed + Chutes re-attached (never dropped).
        assert_eq!(
            merged.get("near+chutes"),
            Some(&vec![ProviderTier::Near, ProviderTier::Attested3p])
        );
        // A pinned model NOT in this batch must NOT be emitted (left untouched) —
        // the partial-batch safety the admin PATCH path depends on.
        assert!(!merged.contains_key("other-pinned"));
        assert_eq!(merged.len(), 1);
    }

    /// The refresh failure-counter prune (review round 3, Pierre's blocking) must
    /// NOT reset a still-served pinned (Chutes) fallback's consecutive-failure
    /// counter. `old_provider_ptrs` over-approximates "replaced" — a pinned provider
    /// lands in it every refresh (it's never in the URL-cache reused set) yet stays
    /// in `model_to_providers` — so the prune is filtered against the post-merge live
    /// pointers. Without that filter a persistently-failing Chutes fallback would be
    /// re-promoted every ~5 min and could never stay demoted.
    #[test]
    fn prune_replaced_failure_counts_keeps_live_pinned_and_shared_drops_replaced() {
        use std::collections::{HashMap, HashSet};

        // 1 = replaced NEAR provider: a candidate, no longer live  -> pruned
        // 2 = pinned Chutes fallback: a candidate (not in reused set) but STILL live
        //     after the merge                                       -> kept
        // 3 = Arc shared with another still-served model: candidate but live -> kept
        // 4 = untouched provider for an unrelated model: not a candidate    -> kept
        let mut counts: HashMap<usize, u32> = [(1usize, 12u32), (2, 9), (3, 7), (4, 3)]
            .into_iter()
            .collect();
        let candidates: HashSet<usize> = [1usize, 2, 3].into_iter().collect();
        let live: HashSet<usize> = [2usize, 3, 4].into_iter().collect();

        InferenceProviderPool::prune_replaced_failure_counts(&mut counts, &candidates, &live);

        assert!(
            !counts.contains_key(&1),
            "a genuinely-replaced provider's counter must be pruned"
        );
        assert_eq!(
            counts.get(&2),
            Some(&9),
            "a still-served pinned fallback must keep its counter (would have caught the round-3 bug)"
        );
        assert_eq!(
            counts.get(&3),
            Some(&7),
            "a still-referenced shared Arc must keep its counter"
        );
        assert_eq!(
            counts.get(&4),
            Some(&3),
            "an untouched provider's counter must survive"
        );
    }

    /// Complete-set stale-prune (review round 2): on the refresh path, a pinned id
    /// absent from the COMPLETE discovery set is rebuilt pinned-only (dropping a
    /// stale NEAR backend), while one still present is left coexisting.
    #[tokio::test]
    async fn prune_stale_pinned_rebuilds_absent_pinned_only() {
        use inference_providers::mock::MockProvider;
        use inference_providers::ProviderTier;
        use std::collections::HashSet;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model = "zai-org/GLM-5.1-FP8".to_string();
        pool.register_provider(
            model.clone(),
            Arc::new(MockProvider::new().with_tier(ProviderTier::Near)),
        )
        .await;
        pool.register_pinned_secondary_provider(
            model.clone(),
            Arc::new(MockProvider::new().with_tier(ProviderTier::Attested3p)),
        )
        .await;

        // Still discovery-served → coexistence preserved.
        let mut complete = HashSet::new();
        complete.insert(model.clone());
        pool.prune_stale_pinned(&complete).await;
        let tiers: Vec<ProviderTier> = pool
            .get_providers_with_fallback(&model, None)
            .await
            .expect("providers")
            .iter()
            .map(|p| p.tier())
            .collect();
        assert_eq!(tiers, vec![ProviderTier::Near, ProviderTier::Attested3p]);

        // NEAR dropped it (absent from the complete set) → rebuilt pinned-only.
        pool.prune_stale_pinned(&HashSet::new()).await;
        let tiers: Vec<ProviderTier> = pool
            .get_providers_with_fallback(&model, None)
            .await
            .expect("providers")
            .iter()
            .map(|p| p.tier())
            .collect();
        assert_eq!(tiers, vec![ProviderTier::Attested3p]);
    }

    /// The streaming-operations list is explicit (review round 2 minor): the known
    /// streaming op routed through `retry_with_fallback` must be registered, so a
    /// rename/new op can't silently bypass the streaming-capability filter.
    #[test]
    fn streaming_operations_list_is_explicit() {
        assert!(InferenceProviderPool::STREAMING_OPERATIONS.contains(&"chat_completion_stream"));
    }

    /// Client-E2EE capability filter (review round 3 blocking): when a request needs
    /// client-facing E2EE and a capable (NEAR) provider exists, a non-capable Chutes
    /// fallback is dropped — so a NEAR failure can't fall through to Chutes' hard
    /// "client E2EE not supported" rejection. When the request doesn't need it, or no
    /// provider supports it, the list is unchanged (clear error still surfaces).
    #[test]
    fn filter_client_e2ee_capable_prefers_capable_providers() {
        use inference_providers::mock::MockProvider;
        use inference_providers::ProviderTier;

        let near = Arc::new(MockProvider::new().with_tier(ProviderTier::Near))
            as Arc<InferenceProviderTrait>;
        let chutes_no_client_e2ee = Arc::new(
            MockProvider::new()
                .with_tier(ProviderTier::Attested3p)
                .with_client_e2ee_support(false),
        ) as Arc<InferenceProviderTrait>;

        // needs client-E2EE, NEAR supports it + Chutes doesn't → Chutes dropped.
        let out = InferenceProviderPool::filter_client_e2ee_capable(
            vec![near.clone(), chutes_no_client_e2ee.clone()],
            true,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tier(), ProviderTier::Near);

        // request doesn't need client-E2EE → no filtering.
        let out = InferenceProviderPool::filter_client_e2ee_capable(
            vec![near.clone(), chutes_no_client_e2ee.clone()],
            false,
        );
        assert_eq!(out.len(), 2);

        // needs it, but the ONLY provider can't → kept (clear rejection surfaces).
        let out = InferenceProviderPool::filter_client_e2ee_capable(
            vec![chutes_no_client_e2ee.clone()],
            true,
        );
        assert_eq!(out.len(), 1);
    }

    /// Reserved-only cleanup (review round 3 important): a fail-closed RESERVATION
    /// (in `pinned_models` but with no actual pinned provider — Chutes failed to
    /// build) must NOT be exempt from stale removal. If NEAR also drops it, it's
    /// removed entirely (fail-closed 404). A real pinned provider still survives.
    #[tokio::test]
    async fn reserved_only_pinned_id_is_removed_when_stale_but_real_pinned_survives() {
        use inference_providers::mock::MockProvider;
        use inference_providers::ProviderTier;
        use std::collections::HashSet;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());

        // Reserved-only: reserved (fail-closed) + a NEAR provider, but Chutes never
        // built, so it is NOT in pinned_providers.
        let reserved_only = "reserved/only".to_string();
        pool.reserve_pinned_models(std::slice::from_ref(&reserved_only));
        pool.register_provider(
            reserved_only.clone(),
            Arc::new(MockProvider::new().with_tier(ProviderTier::Near)),
        )
        .await;

        // Real pinned: has an actual Chutes provider.
        let real_pinned = "real/pinned".to_string();
        pool.register_pinned_secondary_provider(
            real_pinned.clone(),
            Arc::new(MockProvider::new().with_tier(ProviderTier::Attested3p)),
        )
        .await;

        // A refresh where NEAR serves neither (empty valid set).
        pool.remove_stale_providers(&HashSet::new()).await;

        assert!(
            !pool.has_provider(&reserved_only).await,
            "reserved-only id (no pinned provider) must be removed when stale — fail-closed"
        );
        assert!(
            pool.has_provider(&real_pinned).await,
            "a real pinned (Chutes) provider must survive stale removal"
        );
    }

    /// Fail-closed reservation (review #1): a configured Chutes canonical id is
    /// reserved BEFORE external load, so a colliding plaintext external/OpenRouter
    /// row can never register for it — even if the Chutes provider fails to build.
    /// The model serves only attested providers or fails closed (no plaintext).
    #[tokio::test]
    async fn reserved_canonical_id_blocks_plaintext_external_collision() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test-key".to_string()),
                timeout_seconds: 60,
                refresh_interval_secs: 0,
                ..Default::default()
            },
        );
        let model = "zai-org/GLM-5.1-FP8".to_string();

        // Fail-closed: reserve the canonical id up front (Chutes build may fail, so
        // no attested provider is ever registered for it).
        pool.reserve_pinned_models(std::slice::from_ref(&model));

        // A plaintext external row collides on the same canonical id.
        let _ = pool
            .load_external_providers(vec![(
                model.clone(),
                serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"}),
            )])
            .await;

        assert!(
            !pool.has_provider(&model).await,
            "a reserved (verifiable) canonical id must NOT get a plaintext external provider"
        );
    }

    /// Verify that reused providers (URL unchanged) keep their pubkey mappings
    /// after load_inference_url_models refreshes.
    ///
    /// Regression test: previously, reused provider Arc pointers were collected
    /// as "old" and pruned from pubkey_to_providers, but never re-added because
    /// only new providers had their pub_keys collected. This caused E2EE routing
    /// to fail after the first refresh cycle.
    #[tokio::test]
    async fn test_reused_providers_keep_pubkey_mapping() {
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "test-model".to_string();

        // Register a provider with known pubkeys
        let mock_provider = Arc::new(MockProvider::new());
        pool.register_provider(model_id.clone(), mock_provider.clone())
            .await;

        // Verify pubkey routing works initially
        let ecdsa_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        {
            let mappings = pool.provider_mappings.read().await;
            assert!(
                mappings.pubkey_to_providers.contains_key(ecdsa_key),
                "ECDSA key should be in pubkey_to_providers after registration"
            );
            let providers = mappings.pubkey_to_providers.get(ecdsa_key).unwrap();
            assert_eq!(providers.len(), 1);
            assert!(Arc::ptr_eq(
                &providers[0],
                &(mock_provider.clone() as Arc<InferenceProviderTrait>)
            ));
        }

        // Now simulate what load_inference_url_models does when a provider is reused:
        // 1. The same Arc is added to model_providers
        // 2. Old ptrs are collected (including the reused one)
        // 3. pubkey_to_providers is pruned
        // 4. Only NEW provider pubkeys are re-added
        //
        // We simulate this by calling the internal logic path with
        // the same provider being "reused" (same Arc pointer).
        {
            let mut mappings = pool.provider_mappings.write().await;

            // Simulated reused provider
            let reused_provider = mock_provider.clone() as Arc<InferenceProviderTrait>;
            let reused_ptr = Arc::as_ptr(&reused_provider) as *const () as usize;

            // Build reused set (the fix)
            let reused_ptrs: std::collections::HashSet<usize> = [reused_ptr].into_iter().collect();

            // Collect "old" provider ptrs, excluding reused ones
            let mut old_provider_ptrs = std::collections::HashSet::new();
            if let Some(old) = mappings.model_to_providers.get(&model_id) {
                for p in old {
                    let ptr = Arc::as_ptr(p) as *const () as usize;
                    if !reused_ptrs.contains(&ptr) {
                        old_provider_ptrs.insert(ptr);
                    }
                }
            }

            // Replace model providers with "new" list (same Arc)
            mappings
                .model_to_providers
                .insert(model_id.clone(), vec![reused_provider]);

            // Prune old (non-reused) provider pubkeys
            if !old_provider_ptrs.is_empty() {
                mappings.pubkey_to_providers.retain(|_, providers| {
                    providers.retain(|p| {
                        !old_provider_ptrs.contains(&(Arc::as_ptr(p) as *const () as usize))
                    });
                    !providers.is_empty()
                });
            }

            // Verify: reused provider's pubkey mapping should still exist
            assert!(
                mappings.pubkey_to_providers.contains_key(ecdsa_key),
                "ECDSA key should be PRESERVED for reused providers after refresh"
            );
        }
    }

    /// Verify that the self-healing recovery path re-fetches pubkeys for reused
    /// providers that are missing from pubkey_to_providers.
    ///
    /// Regression test: if the initial pubkey fetch failed during provider creation
    /// (transient network error), the provider was cached in inference_url_providers
    /// but had no pubkey mappings. Subsequent refreshes reused the provider and never
    /// retried the pubkey fetch, leaving E2EE permanently broken for that model.
    #[tokio::test]
    async fn test_reused_provider_missing_pubkeys_are_refetched() {
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "test-model".to_string();

        // Register a provider with known pubkeys
        let mock_provider = Arc::new(MockProvider::new());
        pool.register_provider(model_id.clone(), mock_provider.clone())
            .await;

        let ecdsa_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        // Verify pubkeys exist after registration
        {
            let mappings = pool.provider_mappings.read().await;
            assert!(
                mappings.pubkey_to_providers.contains_key(ecdsa_key),
                "ECDSA key should exist after registration"
            );
        }

        // Simulate the bug: clear pubkey mappings (as if initial fetch failed)
        {
            let mut mappings = pool.provider_mappings.write().await;
            mappings.pubkey_to_providers.clear();
        }

        // Seed the URL cache so the provider is "reused" on next load
        let url = "https://test.completions.near.ai".to_string();
        {
            let mut cache = pool.inference_url_providers.write().await;
            cache.insert(
                url.clone(),
                mock_provider.clone() as Arc<InferenceProviderTrait>,
            );
        }

        // Call load_inference_url_models — the provider should be reused and
        // the self-healing path should detect missing pubkeys and re-fetch them.
        pool.load_inference_url_models(vec![(model_id.clone(), url)], false)
            .await;

        // Verify pubkeys were recovered
        {
            let mappings = pool.provider_mappings.read().await;
            assert!(
                mappings.pubkey_to_providers.contains_key(ecdsa_key),
                "ECDSA key should be RECOVERED after refresh via self-healing path"
            );
        }

        // Verify E2EE routing works
        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", Some(ecdsa_key), |_provider| async {
                Ok(())
            })
            .await;
        assert!(
            result.is_ok(),
            "E2EE routing should work after pubkey recovery, got: {:?}",
            result.err()
        );
    }

    /// Test that E2EE routing via pubkey works end-to-end after register_provider.
    /// This exercises: register_provider → fetch attestation → store pubkey → route by pubkey.
    #[tokio::test]
    async fn test_e2ee_pubkey_routing_after_register() {
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "test-e2ee-model".to_string();

        // Register provider (fetches attestation, stores pubkeys)
        let mock_provider = Arc::new(MockProvider::new());
        pool.register_provider(model_id.clone(), mock_provider)
            .await;

        // The mock provider returns this ECDSA key
        let ecdsa_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        // Test 1: routing WITHOUT pubkey should work
        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, |_provider| async { Ok(()) })
            .await;
        assert!(result.is_ok(), "Routing without pubkey should succeed");

        // Test 2: routing WITH correct pubkey should work
        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", Some(ecdsa_key), |_provider| async {
                Ok(())
            })
            .await;
        assert!(
            result.is_ok(),
            "Routing with correct ECDSA pubkey should succeed, got: {:?}",
            result.err()
        );

        // Test 3: routing with WRONG pubkey should fail
        let result: Result<((), _), _> = pool
            .retry_with_fallback(
                &model_id,
                "test_op",
                Some("deadbeef00000000deadbeef00000000deadbeef00000000deadbeef00000000deadbeef00000000deadbeef00000000deadbeef00000000deadbeef00000000"),
                |_provider| async { Ok(()) },
            )
            .await;
        assert!(result.is_err(), "Routing with wrong pubkey should fail");
    }

    #[tokio::test]
    async fn test_sync_external_providers() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
                ..Default::default()
            },
        );

        pool.sync_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
        ]).await;

        assert!(pool.has_provider("gpt-4").await);

        // Sync with partial failures
        pool.sync_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
            ("claude-3".to_string(), serde_json::json!({"backend": "anthropic", "base_url": "https://api.anthropic.com/v1"})),
        ]).await;

        assert!(pool.has_provider("gpt-4").await);
        assert!(!pool.has_provider("claude-3").await);
    }

    // ==================== 4xx Retry Behavior Tests ====================

    /// Helper to create a pool with a registered mock provider
    async fn pool_with_mock_provider() -> (InferenceProviderPool, String) {
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let mock_provider = Arc::new(inference_providers::mock::MockProvider::new());
        let model_id = "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string();
        pool.register_provider(model_id.clone(), mock_provider)
            .await;
        (pool, model_id)
    }

    #[tokio::test]
    async fn test_4xx_error_does_not_retry() {
        let (pool, model_id) = pool_with_mock_provider().await;

        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, |_provider| async {
                Err(CompletionError::HttpError {
                    status_code: 400,
                    message: "Bad request".to_string(),
                    is_external: false,
                })
            })
            .await;

        assert!(result.is_err());
        let err = result.err().expect("Expected an error");
        match err {
            CompletionError::HttpError { status_code, .. } => {
                assert_eq!(status_code, 400);
            }
            other => panic!("Expected HttpError, got: {:?}", other),
        }
    }

    /// Multi-provider, alternating-error test pinning Pierre's blocker: provider
    /// A returns a non-retryable client-media 5xx, provider B (if reached)
    /// would return a retryable 5xx. Without the short-circuit, the for-loop
    /// would walk through both providers; B's `retryable_*` decision would
    /// then flip the outer gate to retry, and the round would loop hitting
    /// provider A with the same bad payload again (~8 attempts across 4
    /// rounds × 2 providers). With the short-circuit, provider A's media
    /// failure returns immediately and B is never tried.
    #[tokio::test(start_paused = true)]
    async fn test_client_media_error_short_circuits_across_providers() {
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string();
        // Register two providers — Pierre's exact scenario shape.
        for _ in 0..2 {
            pool.register_provider(
                model_id.clone(),
                Arc::new(inference_providers::mock::MockProvider::new()),
            )
            .await;
        }

        let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = attempt_count.clone();

        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, move |_provider| {
                let count = count_clone.clone();
                async move {
                    let n = count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n == 0 {
                        // Provider A: non-retryable client-media 5xx.
                        Err(CompletionError::HttpError {
                            status_code: 500,
                            message: "Internal server error: An exception occurred \
                                      while loading IMAGE data at index 0: cannot \
                                      identify image file <_io.BytesIO ...>"
                                .to_string(),
                            is_external: false,
                        })
                    } else {
                        // Provider B (and any further round) would be a transient 502.
                        Err(CompletionError::HttpError {
                            status_code: 502,
                            message: "Bad gateway".to_string(),
                            is_external: false,
                        })
                    }
                }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "client-media 5xx from the first provider must short-circuit immediately; \
             the second provider must not be tried and the round must not retry"
        );
        // And the returned error must be the typed client-media verdict from the
        // first provider (classified on its raw 500 body, carried so the status
        // layer maps it to 400) — not a 502 from a later provider.
        match result.err().expect("err") {
            CompletionError::ClientMediaError(_) => {}
            other => panic!("Expected ClientMediaError, got: {:?}", other),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_429_error_retries_with_exponential_backoff() {
        let (pool, model_id) = pool_with_mock_provider().await;

        let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = attempt_count.clone();

        // 429 should retry with exponential backoff across all rounds
        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, move |_provider| {
                let count = count_clone.clone();
                async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Err(CompletionError::HttpError {
                        status_code: 429,
                        message: "Rate limit exceeded".to_string(),
                        is_external: false,
                    })
                }
            })
            .await;

        assert!(result.is_err());
        // Should have tried 4 times (1 provider × 4 rounds with exponential backoff)
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::Relaxed),
            4,
            "429 errors should be retried with exponential backoff across all rounds"
        );
        let err = result.err().expect("Expected an error");
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("Provider failed for model"),
            "Expected sanitized error (went through retry path), got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_408_error_retries_with_fallback() {
        let (pool, model_id) = pool_with_mock_provider().await;

        // 408 should NOT short-circuit - it should fall through to the normal retry path
        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, |_provider| async {
                Err(CompletionError::HttpError {
                    status_code: 408,
                    message: "Request timeout".to_string(),
                    is_external: false,
                })
            })
            .await;

        assert!(result.is_err());
        let err = result.err().expect("Expected an error");
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("Provider failed for model"),
            "Expected sanitized error (went through retry path), got: {}",
            err_msg
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_connection_error_retries_with_exponential_backoff() {
        let (pool, model_id) = pool_with_mock_provider().await;

        let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = attempt_count.clone();

        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, move |_provider| {
                let count = count_clone.clone();
                async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Err(CompletionError::CompletionError(
                        "error sending request: connection refused".to_string(),
                    ))
                }
            })
            .await;

        assert!(result.is_err());
        // Should have tried 4 times (1 provider × 4 rounds with exponential backoff)
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::Relaxed),
            4,
            "Connection errors should be retried with exponential backoff"
        );
    }

    #[tokio::test]
    async fn test_non_connection_error_does_not_retry() {
        let (pool, model_id) = pool_with_mock_provider().await;

        let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = attempt_count.clone();

        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, move |_provider| {
                let count = count_clone.clone();
                async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Err(CompletionError::CompletionError(
                        "Failed to parse JSON response".to_string(),
                    ))
                }
            })
            .await;

        assert!(result.is_err());
        // Non-connection errors should NOT be retried
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "Non-connection errors should not be retried"
        );
    }

    /// Per-call timeouts surface as `CompletionError::Timeout` and must NOT
    /// trigger the retry loop: re-sending the same request to the same backend
    /// hits the same wall, and 4× a 600s timeout would be 40 minutes of waste.
    #[tokio::test]
    async fn test_timeout_error_does_not_retry() {
        let (pool, model_id) = pool_with_mock_provider().await;

        let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = attempt_count.clone();

        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, move |_provider| {
                let count = count_clone.clone();
                async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Err(CompletionError::Timeout {
                        operation: "chat_completion".to_string(),
                        timeout_seconds: 600,
                    })
                }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "Timeout errors must short-circuit the retry loop"
        );
        // Variant must survive sanitization for the route handler to map it to 504.
        match result.err().expect("Expected an error") {
            CompletionError::Timeout {
                operation,
                timeout_seconds,
            } => {
                assert_eq!(operation, "chat_completion");
                assert_eq!(timeout_seconds, 600);
            }
            other => panic!("Expected CompletionError::Timeout, got: {:?}", other),
        }
    }

    /// Connect-level timeouts surface as string-form errors containing both
    /// "operation timed out" and "connect". They must remain retryable — the
    /// request hadn't reached the backend yet, so a retry has a real shot at
    /// succeeding (different bucket, fresh attestation).
    #[tokio::test(start_paused = true)]
    async fn test_connect_timeout_string_is_retryable() {
        let (pool, model_id) = pool_with_mock_provider().await;

        let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = attempt_count.clone();

        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, move |_provider| {
                let count = count_clone.clone();
                async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Err(CompletionError::CompletionError(
                        "error sending request for url (https://x): operation timed out (connect)"
                            .to_string(),
                    ))
                }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::Relaxed),
            4,
            "connect-timeout should retry the full 4 rounds (1 initial + 3 retries)"
        );
    }

    /// A timeout that arrives via `CompletionError::CompletionError(msg)` (e.g. an
    /// external provider that didn't get the new variant treatment) is also
    /// non-retryable — same logic applies regardless of which variant wraps it.
    #[tokio::test]
    async fn test_string_form_timeout_does_not_retry() {
        let (pool, model_id) = pool_with_mock_provider().await;

        let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = attempt_count.clone();

        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, move |_provider| {
                let count = count_clone.clone();
                async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Err(CompletionError::CompletionError(
                        "error sending request: operation timed out".to_string(),
                    ))
                }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "String-form 'operation timed out' errors must not be retried"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_5xx_error_retries_with_exponential_backoff() {
        let (pool, model_id) = pool_with_mock_provider().await;

        let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = attempt_count.clone();

        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, move |_provider| {
                let count = count_clone.clone();
                async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Err(CompletionError::HttpError {
                        status_code: 502,
                        message: "Bad gateway".to_string(),
                        is_external: false,
                    })
                }
            })
            .await;

        assert!(result.is_err());
        // 5xx should be retried with exponential backoff (4 rounds)
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::Relaxed),
            4,
            "5xx errors should be retried with exponential backoff"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_retry_succeeds_on_second_attempt() {
        let (pool, model_id) = pool_with_mock_provider().await;

        let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = attempt_count.clone();

        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, move |_provider| {
                let count = count_clone.clone();
                async move {
                    let n = count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n == 0 {
                        Err(CompletionError::CompletionError(
                            "error sending request: connection refused".to_string(),
                        ))
                    } else {
                        Ok(())
                    }
                }
            })
            .await;

        assert!(result.is_ok(), "Should succeed on retry");
        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::Relaxed), 2,);
    }

    #[tokio::test]
    async fn test_4xx_error_is_sanitized() {
        let (pool, model_id) = pool_with_mock_provider().await;

        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, |_provider| async {
                Err(CompletionError::HttpError {
                    status_code: 400,
                    message: "Error at http://192.168.0.1:8000/v1/chat/completions".to_string(),
                    is_external: false,
                })
            })
            .await;

        assert!(result.is_err());
        let err = result.err().expect("Expected an error");
        match err {
            CompletionError::HttpError { message, .. } => {
                assert!(
                    !message.contains("192.168.0.1"),
                    "Error message should be sanitized, but contained IP: {}",
                    message
                );
                assert!(
                    message.contains("[URL_REDACTED]"),
                    "Error message should contain redacted URL marker: {}",
                    message
                );
            }
            other => panic!("Expected HttpError, got: {:?}", other),
        }
    }

    /// Verify that when pubkey re-fetch fails for a reused provider (e.g., because
    /// the provider's TLS connections are blocked), the provider is evicted from the
    /// URL cache so it gets recreated from scratch on the next refresh cycle.
    ///
    /// Regression test for the staging deadlock: when all attestation discovery calls
    /// fail during provider creation, block_connections() is called. The blocked
    /// provider is cached, and on subsequent refreshes it's "reused" — but the
    /// re-fetch goes through the same blocked provider, failing forever.
    #[tokio::test]
    async fn test_blocked_provider_evicted_from_cache_on_refetch_failure() {
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "test-blocked-model".to_string();
        let url = "https://blocked-test.completions.near.ai".to_string();

        // Create a mock provider and register it with pubkeys
        let mock_provider = Arc::new(MockProvider::new());
        pool.register_provider(model_id.clone(), mock_provider.clone())
            .await;

        // Seed the URL cache so it's "reused" on next load
        {
            let mut cache = pool.inference_url_providers.write().await;
            cache.insert(
                url.clone(),
                mock_provider.clone() as Arc<InferenceProviderTrait>,
            );
        }

        // Clear pubkey mappings (simulates initial fetch failure)
        {
            let mut mappings = pool.provider_mappings.write().await;
            mappings.pubkey_to_providers.clear();
        }

        // Now make attestation fail (simulates blocked provider)
        mock_provider.set_fail_attestation(true);

        // Load — the provider is reused, pubkeys are missing, re-fetch fails
        pool.load_inference_url_models(vec![(model_id.clone(), url.clone())], false)
            .await;

        // The URL should have been evicted from the cache
        {
            let cache = pool.inference_url_providers.read().await;
            assert!(
                !cache.contains_key(&url),
                "Blocked provider URL should be evicted from cache after failed re-fetch, \
                 but it's still present. This means the provider will be 'reused' forever \
                 and never recreated."
            );
        }

        // The evicted model should also be removed from model_to_providers
        // so it doesn't serve requests with a blocked provider during this cycle.
        {
            let mappings = pool.provider_mappings.read().await;
            assert!(
                !mappings.model_to_providers.contains_key(&model_id),
                "Evicted model should be removed from model_to_providers"
            );
        }

        // Simulate next refresh cycle — now the URL is not in the cache,
        // so it goes through needs_creation (fresh bootstrap TLS provider).
        // The nearai::Provider creation will fail (test URL not reachable), but
        // the important thing is it was NOT reused from the blocked cache.
        let cache_before = {
            let cache = pool.inference_url_providers.read().await;
            cache.contains_key(&url)
        };
        assert!(
            !cache_before,
            "URL should still be absent from cache before second load"
        );
    }

    /// A reused provider whose per-URL `FingerprintState` is `Blocked` cannot
    /// recover via cumulative discovery (every TLS handshake would be rejected
    /// by the pinned verifier). The refresh must detect the Blocked state,
    /// short-circuit before making network calls, and evict all three
    /// tracking maps so the next cycle creates a fresh Bootstrap provider.
    #[tokio::test]
    async fn test_reused_provider_with_blocked_fingerprint_state_is_evicted() {
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "test-blocked-state-model".to_string();
        let url = "https://blocked-state.completions.near.ai".to_string();

        let mock = Arc::new(MockProvider::new());
        pool.register_provider(model_id.clone(), mock.clone()).await;

        // Seed URL cache, tracked fingerprint state (Blocked), and pubkey
        // mappings — so the reused path has everything it needs and would
        // otherwise not trip the legacy refetch branch.
        {
            let mut cache = pool.inference_url_providers.write().await;
            cache.insert(url.clone(), mock.clone() as Arc<InferenceProviderTrait>);
        }
        {
            let mut states = pool.inference_url_fingerprint_states.write().await;
            states.insert(
                url.clone(),
                Arc::new(std::sync::RwLock::new(FingerprintState::Blocked)),
            );
        }
        {
            let mut mappings = pool.provider_mappings.write().await;
            mappings
                .pubkey_to_providers
                .insert("pretend-pubkey".to_string(), vec![mock.clone()]);
        }

        pool.load_inference_url_models(vec![(model_id.clone(), url.clone())], false)
            .await;

        // Blocked URL evicted from URL cache
        {
            let cache = pool.inference_url_providers.read().await;
            assert!(
                !cache.contains_key(&url),
                "URL with Blocked fingerprint state should be evicted from URL cache"
            );
        }
        // Evicted from fingerprint state map too
        {
            let states = pool.inference_url_fingerprint_states.read().await;
            assert!(
                !states.contains_key(&url),
                "URL with Blocked fingerprint state should be evicted from fingerprint_states map"
            );
        }
        // Model removed from provider mappings
        {
            let mappings = pool.provider_mappings.read().await;
            assert!(
                !mappings.model_to_providers.contains_key(&model_id),
                "Model backed by a Blocked provider should be removed from model_to_providers"
            );
        }
    }

    // -------------------------------------------------------------------
    // Fast-path tests for `PoolBackendVerifier`
    //
    // The fast path runs an HTTP probe against `/v1/models` and returns
    // the client without re-attestation when the TLS handshake succeeds.
    // These tests use plain HTTP (the rustls verifier is only consulted
    // for HTTPS URLs, so the TLS-pinning layer is short-circuited) and a
    // hand-rolled TCP responder — same pattern as
    // `crates/inference_providers/src/vllm/mod.rs`. The goal is to verify
    // the control flow (Bootstrap → skip fast path; pinned + 200 → return;
    // pinned + 5xx → fall back; pinned + hang → time out and fall back),
    // not the TLS verifier itself which has its own tests.
    // -------------------------------------------------------------------

    use inference_providers::spki_verifier::SharedTlsRoots;
    use inference_providers::BackendVerifier as _;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Behavior of the mock HTTP server when `/v1/models` is hit.
    #[derive(Clone, Copy)]
    enum ModelsBehavior {
        /// Reply with the given status code and body.
        Reply(u16, &'static str),
        /// Accept the TCP connection but never reply — exercises the
        /// 5-second probe timeout.
        Hang,
    }

    struct FastPathTestServer {
        addr: std::net::SocketAddr,
        models_hits: Arc<AtomicUsize>,
        attestation_hits: Arc<AtomicUsize>,
        _acceptor: tokio::task::JoinHandle<()>,
    }

    /// Spawn a minimal HTTP/1.1 responder. `/v1/attestation/report` always
    /// returns 500 (so the slow-path call from the Bootstrap test errors
    /// out quickly); `/v1/models` is governed by `models_behavior`.
    async fn start_fast_path_server(models_behavior: ModelsBehavior) -> FastPathTestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let models_hits = Arc::new(AtomicUsize::new(0));
        let attestation_hits = Arc::new(AtomicUsize::new(0));
        let m = models_hits.clone();
        let a = attestation_hits.clone();
        let acceptor = tokio::spawn(async move {
            // Sockets that we choose to leave hanging — kept alive so the
            // peer reads "no data yet" rather than an immediate EOF.
            let mut held = Vec::new();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 1024];
                let n = match sock.read(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => continue,
                };
                let head = String::from_utf8_lossy(&buf[..n.min(256)]);
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("");
                if path.starts_with("/v1/models") {
                    m.fetch_add(1, AtomicOrdering::SeqCst);
                    match models_behavior {
                        ModelsBehavior::Reply(status, body) => {
                            let resp = format!(
                                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = sock.write_all(resp.as_bytes()).await;
                        }
                        ModelsBehavior::Hang => {
                            held.push(sock);
                        }
                    }
                } else if path.starts_with("/v1/attestation") {
                    a.fetch_add(1, AtomicOrdering::SeqCst);
                    let body = "{\"error\":\"test\"}";
                    let resp = format!(
                        "HTTP/1.1 500 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            }
        });
        FastPathTestServer {
            addr,
            models_hits,
            attestation_hits,
            _acceptor: acceptor,
        }
    }

    fn pinned_state(fps: &[&str]) -> FingerprintState {
        let mut s = FingerprintState::Bootstrap;
        for fp in fps {
            s.add_fingerprint((*fp).to_string());
        }
        s
    }

    fn make_verifier(state: FingerprintState) -> PoolBackendVerifier {
        PoolBackendVerifier {
            api_key: None,
            model_name: "test-model".to_string(),
            tls_roots: SharedTlsRoots::load(),
            attestation_verifier: Arc::new(AttestationVerifier::new(HashSet::new(), None, false)),
            fingerprint_state: Arc::new(std::sync::RwLock::new(state)),
        }
    }

    #[tokio::test]
    async fn fast_path_returns_client_on_200() {
        let server = start_fast_path_server(ModelsBehavior::Reply(200, "{}")).await;
        let verifier = make_verifier(pinned_state(&["aa", "bb"]));
        let base_url = format!("http://{}", server.addr);
        let result = verifier
            .try_pinned_fast_path(&base_url, pinned_state(&["aa", "bb"]))
            .await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert_eq!(server.models_hits.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(server.attestation_hits.load(AtomicOrdering::SeqCst), 0);
    }

    #[tokio::test]
    async fn fast_path_returns_err_on_http_5xx() {
        let server = start_fast_path_server(ModelsBehavior::Reply(503, "down")).await;
        let verifier = make_verifier(pinned_state(&["aa"]));
        let base_url = format!("http://{}", server.addr);
        let result = verifier
            .try_pinned_fast_path(&base_url, pinned_state(&["aa"]))
            .await;
        let err = result.expect_err("expected Err on HTTP 503");
        assert!(
            err.contains("503"),
            "err should mention status code, got: {err}"
        );
        assert_eq!(server.models_hits.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn create_verified_client_skips_fast_path_in_bootstrap() {
        // Bootstrap state → fast path must not be invoked, slow path runs
        // instead. We don't care that the slow path fails (mock /v1/attestation
        // returns 500); we only assert which endpoint(s) were hit.
        let server = start_fast_path_server(ModelsBehavior::Reply(200, "{}")).await;
        let verifier = make_verifier(FingerprintState::Bootstrap);
        let base_url = format!("http://{}", server.addr);
        let _ = verifier.create_verified_client(&base_url).await;
        assert_eq!(
            server.models_hits.load(AtomicOrdering::SeqCst),
            0,
            "fast path probe must not run when fingerprint_state is Bootstrap"
        );
        assert!(
            server.attestation_hits.load(AtomicOrdering::SeqCst) >= 1,
            "slow path should have attempted /v1/attestation/report"
        );
    }

    #[tokio::test]
    async fn create_verified_client_uses_fast_path_when_pinned() {
        let server = start_fast_path_server(ModelsBehavior::Reply(200, "{}")).await;
        let verifier = make_verifier(pinned_state(&["aa"]));
        let base_url = format!("http://{}", server.addr);
        let client = verifier
            .create_verified_client(&base_url)
            .await
            .expect("fast path should succeed");
        // Successful fast path means the slow path is never reached.
        assert_eq!(server.models_hits.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(
            server.attestation_hits.load(AtomicOrdering::SeqCst),
            0,
            "slow path must not run when fast path succeeds"
        );
        drop(client);
    }

    /// Probe must time out within ~5 s when the backend accepts the
    /// connection but never replies, and the error message must surface
    /// the timeout reason so the fallback debug log is informative.
    #[tokio::test]
    async fn fast_path_returns_err_on_timeout() {
        let server = start_fast_path_server(ModelsBehavior::Hang).await;
        let verifier = make_verifier(pinned_state(&["aa"]));
        let base_url = format!("http://{}", server.addr);
        let start = std::time::Instant::now();
        let result = verifier
            .try_pinned_fast_path(&base_url, pinned_state(&["aa"]))
            .await;
        let elapsed = start.elapsed();
        let err = result.expect_err("expected Err on hanging probe");
        assert!(
            err.contains("timed out"),
            "err should mention timeout, got: {err}"
        );
        // The probe budget is 5 s. Allow scheduler jitter but make sure
        // we're not waiting on a longer timeout by mistake.
        assert!(
            elapsed < Duration::from_secs(7),
            "probe should give up within ~5s, took {elapsed:?}"
        );
        assert_eq!(server.models_hits.load(AtomicOrdering::SeqCst), 1);
    }

    // ==================== Embeddings Error Propagation Tests ====================

    #[tokio::test]
    async fn test_embeddings_preserves_http_error_status_code() {
        // Upstream rejects an embedding request with HTTP 400 (e.g. unsupported
        // `dimensions` param). The pool MUST preserve the HttpError variant so
        // the route can return HTTP 400 — not collapse it to RequestFailed,
        // which previously caused every upstream error to surface as HTTP 502.
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "Qwen/Qwen3-Embedding-0.6B".to_string();
        let mock = Arc::new(MockProvider::new_accept_all());
        mock.set_embedding_error_override(Some(inference_providers::EmbeddingError::HttpError {
            status_code: 400,
            message: "dimensions parameter is not supported for this model".to_string(),
        }))
        .await;
        pool.register_provider(model_id.clone(), mock).await;

        let body = bytes::Bytes::from(
            r#"{"model":"Qwen/Qwen3-Embedding-0.6B","input":"hi","dimensions":256}"#,
        );
        let result = pool
            .embeddings(&model_id, body, std::collections::HashMap::new())
            .await;

        match result {
            Err(inference_providers::EmbeddingError::HttpError {
                status_code,
                message,
            }) => {
                assert_eq!(status_code, 400);
                assert!(
                    message.contains("dimensions parameter is not supported"),
                    "Expected upstream message to be preserved, got: {message}"
                );
            }
            other => panic!("Expected HttpError(400, ...), got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_embeddings_request_failed_stays_request_failed() {
        // Non-HTTP errors (e.g. network/connection failure) should still
        // surface as RequestFailed so the route maps them to 502.
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "Qwen/Qwen3-Embedding-0.6B".to_string();
        let mock = Arc::new(MockProvider::new_accept_all());
        mock.set_embedding_error_override(Some(
            inference_providers::EmbeddingError::RequestFailed(
                "connection reset by peer".to_string(),
            ),
        ))
        .await;
        pool.register_provider(model_id.clone(), mock).await;

        let result = pool
            .embeddings(
                &model_id,
                bytes::Bytes::from(r#"{"model":"x","input":"hi"}"#),
                std::collections::HashMap::new(),
            )
            .await;

        match result {
            Err(inference_providers::EmbeddingError::RequestFailed(msg)) => {
                assert!(msg.contains("connection reset"));
            }
            other => panic!("Expected RequestFailed, got: {:?}", other),
        }
    }

    // ==================== Per-request NEAR→Chutes fallback ====================
    //
    // These exercise the END-TO-END per-request fallback through
    // `chat_completion` (→ `retry_with_fallback_caps`), the behavior a staging
    // backend-drain could NOT validate: cloud-api holds sticky keepalive
    // connections to NEAR backends, so a model-proxy registry change never makes
    // the live request fail. The fallback only triggers on a genuine
    // request-level failure, which is deterministic to inject with a mock.

    fn fallback_params(model: &str) -> inference_providers::ChatCompletionParams {
        inference_providers::ChatCompletionParams {
            model: model.to_string(),
            messages: vec![inference_providers::ChatMessage {
                role: inference_providers::MessageRole::User,
                content: Some(serde_json::Value::String("hello".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: None,
            stream: Some(false),
            tools: None,
            max_completion_tokens: None,
            n: None,
            frequency_penalty: None,
            presence_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: None,
            seed: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: None,
            store: None,
            stream_options: None,
            modalities: None,
            extra: std::collections::HashMap::new(),
        }
    }

    /// A retryable NEAR primary failure (upstream 5xx) MUST fall through to the
    /// Chutes (Attested3p) provider within the SAME request — the client sees a
    /// success, not the NEAR error. Also asserts NEAR is attempted first (it's
    /// the primary tier) and that the fallback is immediate (no long backoff
    /// before trying the next tier).
    #[tokio::test]
    async fn near_5xx_falls_back_to_chutes_within_one_request() {
        use inference_providers::mock::{MockProvider, RequestMatcher, ResponseTemplate};
        use inference_providers::{CompletionError, ProviderTier};

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "z-ai/glm-5.1".to_string();

        // NEAR primary: always returns a retryable upstream 5xx.
        let near = Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Near));
        near.set_error_override(Some(CompletionError::HttpError {
            status_code: 503,
            message: "backend overloaded".to_string(),
            is_external: true,
        }))
        .await;

        // Chutes fallback: healthy, returns a recognizable response.
        let chutes = Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Attested3p));
        chutes
            .when(RequestMatcher::Any)
            .respond_with(ResponseTemplate::new("served-by-chutes-fallback"))
            .await;

        {
            let mut m = pool.provider_mappings.write().await;
            m.model_to_providers.insert(
                model_id.clone(),
                vec![
                    near.clone() as Arc<InferenceProviderTrait>,
                    chutes.clone() as Arc<InferenceProviderTrait>,
                ],
            );
        }

        let started = std::time::Instant::now();
        let resp = pool
            .chat_completion(fallback_params(&model_id), "test-hash".to_string())
            .await
            .expect("NEAR 5xx must fall back to Chutes, not fail the client request");
        let elapsed = started.elapsed();

        // NEAR was attempted first (primary tier), not skipped.
        assert!(
            near.last_chat_params().await.is_some(),
            "NEAR (primary) must be attempted before falling back"
        );
        // Chutes actually served the request.
        assert!(
            chutes.last_chat_params().await.is_some(),
            "Chutes must serve the fallback after the NEAR 5xx"
        );
        let body = String::from_utf8_lossy(&resp.raw_bytes);
        assert!(
            body.contains("served-by-chutes-fallback"),
            "response must be the Chutes one, got: {body}"
        );
        // Fallback is within the same round (next provider tried immediately) —
        // no client-facing retry/backoff latency.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "fallback should be immediate, took {elapsed:?}"
        );
    }

    /// When the NEAR primary AND the Chutes fallback both fail with a retryable
    /// 5xx, the error must SURFACE to the client — the pool must not invent a
    /// false success once every provider is exhausted.
    #[tokio::test]
    async fn all_providers_5xx_surfaces_error() {
        use inference_providers::mock::MockProvider;
        use inference_providers::{CompletionError, ProviderTier};

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "z-ai/glm-5.1".to_string();
        let err = || CompletionError::HttpError {
            status_code: 503,
            message: "down".to_string(),
            is_external: true,
        };

        let near = Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Near));
        near.set_error_override(Some(err())).await;
        let chutes = Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Attested3p));
        chutes.set_error_override(Some(err())).await;

        {
            let mut m = pool.provider_mappings.write().await;
            m.model_to_providers.insert(
                model_id.clone(),
                vec![
                    near.clone() as Arc<InferenceProviderTrait>,
                    chutes.clone() as Arc<InferenceProviderTrait>,
                ],
            );
        }

        let result = pool
            .chat_completion(fallback_params(&model_id), "test-hash".to_string())
            .await;
        assert!(
            result.is_err(),
            "all providers failing must surface an error, not a false success"
        );
        // Both tiers were genuinely attempted.
        assert!(near.last_chat_params().await.is_some());
        assert!(chutes.last_chat_params().await.is_some());
    }

    /// A healthy NEAR primary serves the request itself; the Chutes fallback must
    /// NOT be invoked when the primary succeeds (no needless fallback / billing).
    #[tokio::test]
    async fn healthy_near_serves_without_invoking_chutes() {
        use inference_providers::mock::{MockProvider, RequestMatcher, ResponseTemplate};
        use inference_providers::ProviderTier;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "z-ai/glm-5.1".to_string();

        let near = Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Near));
        near.when(RequestMatcher::Any)
            .respond_with(ResponseTemplate::new("served-by-near-primary"))
            .await;
        let chutes = Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Attested3p));

        {
            let mut m = pool.provider_mappings.write().await;
            m.model_to_providers.insert(
                model_id.clone(),
                vec![
                    near.clone() as Arc<InferenceProviderTrait>,
                    chutes.clone() as Arc<InferenceProviderTrait>,
                ],
            );
        }

        let resp = pool
            .chat_completion(fallback_params(&model_id), "test-hash".to_string())
            .await
            .expect("healthy NEAR must serve");
        let body = String::from_utf8_lossy(&resp.raw_bytes);
        assert!(
            body.contains("served-by-near-primary"),
            "NEAR must serve, got: {body}"
        );
        assert!(
            chutes.last_chat_params().await.is_none(),
            "Chutes must NOT be invoked when the NEAR primary succeeds"
        );
    }

    /// nearai/cloud-api#797: a NEAR provider that returns a "model not found"
    /// 4xx (OpenAI-compatible engines do this for an unrecognized model id, e.g.
    /// a renamed canonical the backend hasn't picked up) must fall through to the
    /// Chutes fallback — NOT fast-return as a client error. Both 404 and 400
    /// phrasings are exercised.
    #[tokio::test]
    async fn near_model_not_found_falls_back_to_chutes() {
        use inference_providers::mock::{MockProvider, RequestMatcher, ResponseTemplate};
        use inference_providers::{CompletionError, ProviderTier};

        for (status_code, message) in [
            (404u16, "The model `z-ai/glm-5.1` does not exist."),
            (400u16, "The model `z-ai/glm-5.1` does not exist."),
        ] {
            let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
            let model_id = "z-ai/glm-5.1".to_string();

            let near = Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Near));
            near.set_error_override(Some(CompletionError::HttpError {
                status_code,
                message: message.to_string(),
                is_external: true,
            }))
            .await;

            let chutes =
                Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Attested3p));
            chutes
                .when(RequestMatcher::Any)
                .respond_with(ResponseTemplate::new("served-by-chutes-fallback"))
                .await;

            {
                let mut m = pool.provider_mappings.write().await;
                m.model_to_providers.insert(
                    model_id.clone(),
                    vec![
                        near.clone() as Arc<InferenceProviderTrait>,
                        chutes.clone() as Arc<InferenceProviderTrait>,
                    ],
                );
            }

            let resp = pool
                .chat_completion(fallback_params(&model_id), "test-hash".to_string())
                .await
                .unwrap_or_else(|e| {
                    panic!("NEAR {status_code} model-not-found must fall back to Chutes, got: {e}")
                });
            assert!(
                near.last_chat_params().await.is_some(),
                "NEAR must be attempted first ({status_code})"
            );
            assert!(
                chutes.last_chat_params().await.is_some(),
                "Chutes must serve after NEAR model-not-found ({status_code})"
            );
            let body = String::from_utf8_lossy(&resp.raw_bytes);
            assert!(
                body.contains("served-by-chutes-fallback"),
                "response must be Chutes's ({status_code}), got: {body}"
            );
        }
    }

    /// Guard against over-reaching the #797 fix: a GENUINE client 4xx (bad
    /// params, not model-not-found) must still fast-return without trying the
    /// fallback — retrying an invalid request on Chutes would only waste it.
    #[tokio::test]
    async fn genuine_client_4xx_does_not_fall_back() {
        use inference_providers::mock::MockProvider;
        use inference_providers::{CompletionError, ProviderTier};

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "z-ai/glm-5.1".to_string();

        let near = Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Near));
        near.set_error_override(Some(CompletionError::HttpError {
            status_code: 400,
            message: "max_tokens exceeds the model's context length".to_string(),
            is_external: true,
        }))
        .await;
        let chutes = Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Attested3p));

        {
            let mut m = pool.provider_mappings.write().await;
            m.model_to_providers.insert(
                model_id.clone(),
                vec![
                    near.clone() as Arc<InferenceProviderTrait>,
                    chutes.clone() as Arc<InferenceProviderTrait>,
                ],
            );
        }

        let result = pool
            .chat_completion(fallback_params(&model_id), "test-hash".to_string())
            .await;
        assert!(result.is_err(), "a genuine client 4xx must propagate");
        assert!(
            chutes.last_chat_params().await.is_none(),
            "Chutes must NOT be tried for a genuine client 4xx"
        );
    }

    /// Observability: the pool emits `cloud_api.provider.requests` tagged with the
    /// serving tier and a `fallback` flag, so dashboards can see Chutes-served
    /// traffic and the NEAR→fallback rate. A NEAR-5xx→Chutes request must record
    /// `provider_tier:attested_3p, fallback:true`; a healthy NEAR primary must
    /// record `provider_tier:near, fallback:false`.
    #[tokio::test]
    async fn provider_requests_metric_tags_tier_and_fallback() {
        use crate::metrics::capturing::{CapturingMetricsService, MetricValue};
        use crate::metrics::consts::METRIC_PROVIDER_REQUESTS;
        use inference_providers::mock::{MockProvider, RequestMatcher, ResponseTemplate};
        use inference_providers::{CompletionError, ProviderTier};

        // Helper: run one request through a pool wired with capturing metrics and
        // return the recorded provider-requests tag sets.
        async fn served_tags(near_fails: bool) -> Vec<Vec<String>> {
            let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
            let metrics = Arc::new(CapturingMetricsService::new());
            pool.set_metrics_service(metrics.clone());
            let model_id = "z-ai/glm-5.1".to_string();

            let near = Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Near));
            if near_fails {
                near.set_error_override(Some(CompletionError::HttpError {
                    status_code: 503,
                    message: "overloaded".to_string(),
                    is_external: true,
                }))
                .await;
            } else {
                near.when(RequestMatcher::Any)
                    .respond_with(ResponseTemplate::new("near"))
                    .await;
            }
            let chutes =
                Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Attested3p));
            chutes
                .when(RequestMatcher::Any)
                .respond_with(ResponseTemplate::new("chutes"))
                .await;
            {
                let mut m = pool.provider_mappings.write().await;
                m.model_to_providers.insert(
                    model_id.clone(),
                    vec![
                        near.clone() as Arc<InferenceProviderTrait>,
                        chutes.clone() as Arc<InferenceProviderTrait>,
                    ],
                );
            }
            pool.chat_completion(fallback_params(&model_id), "h".to_string())
                .await
                .expect("served");

            metrics
                .get_metrics()
                .into_iter()
                .filter(|m| m.name == METRIC_PROVIDER_REQUESTS)
                .inspect(|m| assert!(matches!(m.value, MetricValue::Count(1))))
                .map(|m| m.tags)
                .collect()
        }

        // NEAR 5xx → served by Chutes fallback.
        let fb = served_tags(true).await;
        assert_eq!(fb.len(), 1, "exactly one provider-requests metric");
        assert!(fb[0].iter().any(|t| t == "provider_tier:attested_3p"));
        assert!(fb[0].iter().any(|t| t == "fallback:true"));
        assert!(fb[0].iter().any(|t| t == "model:z-ai/glm-5.1"));

        // Healthy NEAR → served by primary, no fallback.
        let primary = served_tags(false).await;
        assert_eq!(primary.len(), 1);
        assert!(primary[0].iter().any(|t| t == "provider_tier:near"));
        assert!(primary[0].iter().any(|t| t == "fallback:false"));

    /// When `load_inference_url_models` is called in partial mode (`partial = true`),
    /// it must merge into the existing URL-provider cache and fingerprint-state map
    /// rather than replacing them.  Providers for models not in the input set must
    /// be preserved so they do not need to re-attest on the next periodic refresh.
    ///
    /// Regression test for issue #775: `batch_upsert_models` was calling
    /// `load_inference_url_models` with only the PATCHed models, which caused the
    /// full URL cache and fingerprint-state map to be replaced by a subset, evicting
    /// all untouched providers and forcing unnecessary re-attestation.
    #[tokio::test]
    async fn test_partial_load_preserves_untouched_providers() {
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());

        // Pre-seed the URL cache with two "existing" models as if they were
        // loaded during a prior full sync.
        let _untouched_model = "model-untouched".to_string();
        let untouched_url = "https://untouched.completions.near.ai".to_string();
        let untouched_provider = Arc::new(MockProvider::new());

        let patched_model = "model-patched".to_string();
        let patched_url = "https://patched.completions.near.ai".to_string();
        let patched_provider = Arc::new(MockProvider::new());

        {
            let mut cache = pool.inference_url_providers.write().await;
            cache.insert(
                untouched_url.clone(),
                untouched_provider.clone() as Arc<InferenceProviderTrait>,
            );
            cache.insert(
                patched_url.clone(),
                patched_provider.clone() as Arc<InferenceProviderTrait>,
            );
        }

        // Also seed fingerprint states for both.
        {
            let mut states = pool.inference_url_fingerprint_states.write().await;
            states.insert(
                untouched_url.clone(),
                Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap)),
            );
            states.insert(
                patched_url.clone(),
                Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap)),
            );
        }

        // Partial load — only the patched model is included.
        pool.load_inference_url_models(vec![(patched_model.clone(), patched_url.clone())], true)
            .await;

        // The untouched model's URL must still be present in the URL cache.
        {
            let cache = pool.inference_url_providers.read().await;
            assert!(
                cache.contains_key(&untouched_url),
                "partial load must not evict untouched provider from URL cache"
            );
        }

        // The untouched model's fingerprint state must also be preserved.
        {
            let states = pool.inference_url_fingerprint_states.read().await;
            assert!(
                states.contains_key(&untouched_url),
                "partial load must not prune fingerprint state for untouched provider"
            );
        }
    }
}
