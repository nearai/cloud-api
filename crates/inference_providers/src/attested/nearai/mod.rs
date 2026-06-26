mod fleet;
mod prefix_router;

use crate::spki_verifier::{FingerprintState, SharedTlsRoots};
use crate::{
    models::StreamOptions, sse_parser::new_sse_parser, ImageEditError, ImageGenerationError,
    PrivacyClassifyError, RerankError, ScoreError, *,
};
use async_trait::async_trait;
use fleet::Fleet;
use prefix_router::PrefixRouter;
use reqwest::{header::HeaderValue, Client};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Convert any displayable error to ImageGenerationError::GenerationError
fn to_image_gen_error<E: std::fmt::Display>(e: E) -> ImageGenerationError {
    ImageGenerationError::GenerationError(e.to_string())
}

/// Convert any displayable error to RerankError::GenerationError
fn to_rerank_error<E: std::fmt::Display>(e: E) -> RerankError {
    RerankError::GenerationError(e.to_string())
}

/// Convert any displayable error to ScoreError::GenerationError
fn to_score_error<E: std::fmt::Display>(e: E) -> ScoreError {
    ScoreError::GenerationError(e.to_string())
}

/// Convert any displayable error to EmbeddingError::RequestFailed
fn to_embedding_error<E: std::fmt::Display>(e: E) -> EmbeddingError {
    EmbeddingError::RequestFailed(e.to_string())
}

/// Backoff schedule for retrying a signature fetch that returned 404 on every
/// reachable backend. The backend signs in a background task that finalizes
/// *after* the final stream chunk, then caches the signature; cloud-api fetches
/// in the hot path the instant the stream ends, so an initial 404 ("Chat id not
/// found or expired") is usually a race — the signature lands a few ms later —
/// not a permanent miss.
///
/// Index = number of attempts already completed (0 = wait before the 2nd
/// attempt). The array length bounds retries to `len + 1` total attempts, and
/// the sum (350ms) caps the extra hot-path latency well under the caller's 5s
/// FINALIZE_TIMEOUT. Non-404 statuses and transport errors are definitive and
/// never reach this path.
const SIGNATURE_FETCH_BACKOFFS_MS: [u64; 2] = [100, 250];

/// Backoff to wait before the next signature-fetch retry, or `None` once
/// retries are exhausted. See [`SIGNATURE_FETCH_BACKOFFS_MS`].
fn signature_fetch_backoff(completed_attempts: usize) -> Option<Duration> {
    SIGNATURE_FETCH_BACKOFFS_MS
        .get(completed_attempts)
        .map(|ms| Duration::from_millis(*ms))
}

/// Convert any displayable error to PrivacyClassifyError::RequestFailed
fn to_privacy_classify_error<E: std::fmt::Display>(e: E) -> PrivacyClassifyError {
    PrivacyClassifyError::RequestFailed(e.to_string())
}

/// Format an error including its full `source()` chain.
///
/// `reqwest::Error`'s `Display` impl returns only the outer wrapper
/// (e.g. `"error sending request for url (...)"`). The underlying cause —
/// `"connection closed before message completed"`, `"broken pipe"`,
/// hyper/h2 stream resets, rustls handshake errors — lives in
/// `source()` and is otherwise discarded when we convert to
/// `CompletionError::CompletionError(String)`. Walk the chain so the
/// transport-level reason ends up in logs.
fn format_error_chain<E: std::error::Error>(e: &E) -> String {
    let mut out = e.to_string();
    let mut source: Option<&dyn std::error::Error> = e.source();
    while let Some(err) = source {
        out.push_str(": caused by: ");
        out.push_str(&err.to_string());
        source = err.source();
    }
    out
}

/// Tracing header keys used in params.extra for propagating request correlation IDs.
///
/// These are injected by cloud-api's completion service before calling the inference
/// provider and are forwarded verbatim as HTTP headers to the downstream vllm-proxy /
/// inference-proxy. The values are low-sensitivity org metadata (not user content)
/// so forwarding them is consistent with the TEE trust model.
/// Keys used in `ChatCompletionParams.extra` for tracing correlation headers.
///
/// Values are the snake_case map keys that `prepare_tracing_headers` reads and
/// strips; the corresponding HTTP header names are `X-Request-Id`, `X-Org-Id`,
/// and `X-Workspace-Id`. Exposed as `pub(crate)` so `external/mod.rs` can use
/// the same constants instead of hardcoding the strings.
pub(crate) mod tracing_headers {
    /// UUIDv4 generated per request by cloud-api. Join key across all hops.
    pub const REQUEST_ID: &str = "x_request_id";
    /// Organization UUID of the authenticated API key owner.
    pub const ORG_ID: &str = "x_org_id";
    /// Workspace UUID of the authenticated API key.
    pub const WORKSPACE_ID: &str = "x_workspace_id";
}

/// Encryption header keys used in params.extra for passing encryption information.
/// `pub(crate)` so other providers (e.g. the Chutes path) can strip/reject these
/// internal client-E2EE markers instead of hardcoding the strings.
pub(crate) mod encryption_headers {
    /// Key for signing algorithm (x-signing-algo header)
    pub const SIGNING_ALGO: &str = "x_signing_algo";
    /// Key for client public key (x-client-pub-key header)
    pub const CLIENT_PUB_KEY: &str = "x_client_pub_key";
    /// Key for model public key (x-model-pub-key header)
    /// Note: This is not forwarded to vllm-proxy (vllm-proxy doesn't accept it),
    /// but kept here for consistency with other encryption header constants
    #[allow(dead_code)]
    pub const MODEL_PUB_KEY: &str = "x_model_pub_key";
    /// Key for encryption version (x-encryption-version header)
    pub const ENCRYPTION_VERSION: &str = "x_encryption_version";
    /// Key for full field encryption opt-in (x-encrypt-all-fields header)
    pub const ENCRYPT_ALL_FIELDS: &str = "x_encrypt_all_fields";
}

/// Configuration for vLLM provider.
///
/// Two timeouts are kept independent because they have very different shapes:
/// - **Completion** (chat/text completion, audio, image, embeddings, rerank, score):
///   reasoning models routinely take several minutes per request. The timeout has
///   to be generous enough that the model can finish its CoT before we give up.
/// - **Control** (models list, attestation report, signature fetch, streaming TTFB):
///   these are metadata or first-byte ops that should return promptly. A long timeout
///   here just delays the user's error message when something is actually wrong.
///
/// Both are tunable per-deployment via env vars (see `Config::new`).
#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub api_key: Option<String>,
    /// Total per-request timeout for completion-style operations.
    pub completion_timeout_seconds: i64,
    /// Total per-request timeout for control-plane operations and streaming TTFB.
    pub control_timeout_seconds: i64,
}

impl Config {
    /// Default completion timeout. Reasoning models can spend several minutes
    /// on a single non-streaming request; 600s is a comfortable ceiling that
    /// still surfaces genuinely stuck requests.
    pub const DEFAULT_COMPLETION_TIMEOUT_SECS: i64 = 600;
    /// Default control timeout. Covers TTFB on streaming requests, attestation
    /// report fetches, models-list and signature lookups. Reasoning models
    /// (GLM-5.1, Qwen3.5) can delay TTFB by minutes when the backend queues a
    /// request behind a long thinking phase, and attestation TDX-quote + GPU
    /// evidence collection can also cross 90s under load. 300s gives enough
    /// headroom for those without masking a sustained backend stall.
    pub const DEFAULT_CONTROL_TIMEOUT_SECS: i64 = 300;

    /// Construct a config. The `timeout_seconds` parameter, when supplied, sets
    /// the **completion** timeout only (control stays at its default / env value).
    /// When `None`, both timeouts are read from env vars:
    /// `VLLM_PROVIDER_COMPLETION_TIMEOUT` and `VLLM_PROVIDER_CONTROL_TIMEOUT`.
    pub fn new(base_url: String, api_key: Option<String>, timeout_seconds: Option<i64>) -> Self {
        let completion = timeout_seconds.unwrap_or_else(Self::completion_timeout_from_env);
        let control = Self::control_timeout_from_env();
        Self {
            base_url,
            api_key,
            completion_timeout_seconds: completion,
            control_timeout_seconds: control,
        }
    }

    /// Read the completion timeout from env, falling back to the default.
    pub fn completion_timeout_from_env() -> i64 {
        std::env::var("VLLM_PROVIDER_COMPLETION_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(Self::DEFAULT_COMPLETION_TIMEOUT_SECS)
    }

    /// Read the control timeout from env, falling back to the default.
    pub fn control_timeout_from_env() -> i64 {
        std::env::var("VLLM_PROVIDER_CONTROL_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(Self::DEFAULT_CONTROL_TIMEOUT_SECS)
    }

    pub fn completion_timeout(&self) -> Duration {
        Duration::from_secs(self.completion_timeout_seconds.max(0) as u64)
    }

    pub fn control_timeout(&self) -> Duration {
        Duration::from_secs(self.control_timeout_seconds.max(0) as u64)
    }
}

fn merge_model_responses(responses: Vec<ModelsResponse>) -> ModelsResponse {
    let mut responses = responses.into_iter();
    let Some(mut merged) = responses.next() else {
        return ModelsResponse {
            object: "list".to_string(),
            data: Vec::new(),
        };
    };

    let mut by_id: HashMap<String, usize> = merged
        .data
        .iter()
        .enumerate()
        .map(|(index, model)| (model.id.clone(), index))
        .collect();

    for response in responses {
        for model in response.data {
            if let Some(index) = by_id.get(&model.id).copied() {
                merge_model_metadata(&mut merged.data[index], &model);
            } else {
                by_id.insert(model.id.clone(), merged.data.len());
                merged.data.push(model);
            }
        }
    }

    merged
}

fn merge_model_metadata(existing: &mut ModelInfo, candidate: &ModelInfo) {
    if let Some(context_length) = [
        existing.advertised_context_length(),
        candidate.advertised_context_length(),
    ]
    .into_iter()
    .flatten()
    .max()
    {
        existing.context_length = Some(context_length);
        if let Some(provider) = existing.top_provider.as_mut() {
            provider.context_length = Some(context_length);
        }
    }

    if let Some(max_output_length) = [
        existing.advertised_max_output_length(),
        candidate.advertised_max_output_length(),
    ]
    .into_iter()
    .flatten()
    .max()
    {
        existing.max_output_length = Some(max_output_length);
        if let Some(provider) = existing.top_provider.as_mut() {
            provider.max_completion_tokens = Some(max_output_length);
        }
    }
}

struct ModelsRequest {
    client: Client,
    headers: reqwest::header::HeaderMap,
    timeout: Duration,
    url: String,
}

async fn send_models_request(request: ModelsRequest) -> Result<ModelsResponse, ListModelsError> {
    let response = request
        .client
        .get(&request.url)
        .headers(request.headers)
        .timeout(request.timeout)
        .send()
        .await
        .map_err(|e| ListModelsError::FetchError(format!("{e:?}")))?;

    if !response.status().is_success() {
        return Err(ListModelsError::FetchError(format!(
            "HTTP {}: {}",
            response.status(),
            response.status().canonical_reason().unwrap_or("Unknown")
        )));
    }

    let models_response = response
        .json()
        .await
        .map_err(|_| ListModelsError::InvalidResponse)?;

    Ok(models_response)
}

/// vLLM provider implementation
///
/// Provides inference through vLLM's OpenAI-compatible API endpoints.
/// Supports both chat completions and text completions with streaming.
pub struct Provider {
    /// All NEAR-AI model-proxy state and behavior: config + clients, the TLS
    /// fingerprint pin state, the backend verifier, and the routing state
    /// (prefix affinity, rotation-index addressing, signature pins). Provider is
    /// becoming a thin trait adapter over this; methods currently still on the
    /// provider read their state via `self.fleet.*` until they move too.
    fleet: Arc<Fleet>,
}

/// Client-construction + attestation helpers, owned by Fleet (it holds
/// the config, clients, TLS roots, fingerprint state, and verifier). Moved off
/// Provider in step 4b; the provider's remaining methods call these via
/// `self.fleet.*` until they move in 4c.
impl Fleet {
    /// Block all TLS connections (attestation failed). Only blocks from
    /// Bootstrap — doesn't override an existing Pinned set.
    pub(super) fn block_connections(&self) {
        self.fingerprint_state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .block();
    }

    /// Number of verified fingerprints currently pinned.
    pub(super) fn pinned_fingerprint_count(&self) -> usize {
        self.fingerprint_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .pinned_count()
    }

    /// Whether a CompletionError is a connection/transport failure (vs an
    /// HTTP-level error from the backend).
    pub(super) fn is_connection_error(err: &CompletionError) -> bool {
        match err {
            CompletionError::CompletionError(msg) => {
                msg.contains("error sending request")
                    || msg.contains("connection closed")
                    || msg.contains("connection reset")
                    || msg.contains("broken pipe")
                    || msg.contains("does not match any attested fingerprint")
                    || msg.contains("TLS connections blocked")
            }
            _ => false,
        }
    }

    /// Clear an index's client so it is re-verified on next use (called on a
    /// connection error — a stale H2 connection must not be reused unverified).
    pub(super) fn clear_index(&self, index: usize) {
        *self.index_clients[index]
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Build base HTTP request headers (Content-Type + bearer auth).
    pub(super) fn build_headers(&self) -> Result<reqwest::header::HeaderMap, String> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        if let Some(ref api_key) = self.config.api_key {
            let auth_value = format!("Bearer {api_key}");
            let header_value = HeaderValue::from_str(&auth_value)
                .map_err(|e| format!("Invalid API key format: {e}"))?;
            headers.insert("Authorization", header_value);
        }

        Ok(headers)
    }

    /// Maximum inline-verification retries when creating a verified index client.
    const INLINE_VERIFY_RETRIES: usize = 2;

    /// Spawn background tasks to pre-warm the per-index clients for the live
    /// backend indices (`0..rotation_count()`). No-op without a verifier or
    /// before any fingerprint is pinned (Bootstrap/Blocked) — every task would
    /// otherwise fail the security guard and log noise.
    pub(super) fn pre_warm(self: Arc<Self>) {
        if self.backend_verifier.is_none() {
            return;
        }
        if self.pinned_fingerprint_count() == 0 {
            tracing::debug!(
                "Pre-warm skipped: no fingerprints pinned (Bootstrap or Blocked state)"
            );
            return;
        }
        let count = self.rotation_count();
        if count == 0 {
            tracing::debug!("Pre-warm skipped: rotation count is 0 (no backend count yet)");
            return;
        }
        tracing::info!(num_indices = count, "Pre-warming per-index clients");
        for index in 0..count {
            let fleet = self.clone();
            tokio::spawn(async move {
                match fleet.get_or_verify_index_client(index).await {
                    Ok(_) => tracing::debug!(index, "Index pre-warm complete"),
                    Err(e) => tracing::warn!(
                        index,
                        error = %e,
                        "Index pre-warm failed; will retry inline on first use"
                    ),
                }
            });
        }
    }

    /// Get the client for a backend index, creating + verifying it inline if
    /// needed. Verification targets the index's rotation SNI
    /// (`<canonical>-i<index>.<base>`) so the pinned H2 connection lands on
    /// backend `index`. Bounded by `verification_semaphore`; on exhausted
    /// retries falls back to `fallback_client` only once a fingerprint is
    /// pinned (else fails closed).
    pub(super) async fn get_or_verify_index_client(
        &self,
        index: usize,
    ) -> Result<Client, CompletionError> {
        // Defensive bound. By construction every caller derives `index` from
        // `select_index`/`fallback_indices` (both bounded by `rotation_count()`
        // ≤ `MAX_FANOUT` = `index_clients.len()`) or from a `signature_rotation`
        // pin that was recorded under the same bound — so this never trips
        // today. Guard anyway rather than index-panic: this is attestation
        // hot-path code and a future caller mustn't be able to crash the worker.
        if index >= self.index_clients.len() {
            return Err(CompletionError::CompletionError(format!(
                "backend index {index} out of range (max {})",
                self.index_clients.len()
            )));
        }
        // Fast path: index already has a verified client.
        {
            let guard = self.index_clients[index]
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref client) = *guard {
                return Ok(client.clone());
            }
        }

        // Slow path: inline verification.
        let verifier = match self.backend_verifier.as_ref() {
            Some(v) => v,
            None => {
                return Err(CompletionError::CompletionError(
                    "No backend verifier configured for lazy index creation".to_string(),
                ));
            }
        };

        // Bound concurrent inline verifications (thundering-herd guard). The
        // permit is held for the whole retry loop; the first success fills the
        // index slot and subsequent waiters take the fast path after re-checking.
        let _permit = self
            .verification_semaphore
            .acquire()
            .await
            .expect("verification semaphore should never be closed");

        // Re-check after acquiring the permit.
        {
            let guard = self.index_clients[index]
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref client) = *guard {
                return Ok(client.clone());
            }
        }

        // Verify against the index's rotation SNI so the pinned connection lands
        // on backend `index`. If rotation parts are missing (non-rotation URL),
        // fall back to the canonical base_url — but in that mode callers reach
        // the canonical path and don't request index clients on the hot path.
        //
        // Trim the trailing slash: `rotation_url(index, "")` yields
        // `https://<canonical>-i<index>.<base>/`, and `create_verified_client`
        // appends the probe path as `{base_url}/v1/...`. Without the trim that
        // becomes `//v1/models`, which model-proxy's nginx sidecar does not
        // match — every index client would fail verification in production
        // (localhost tests can't catch this, as they disable rotation). The
        // canonical base_url has no trailing slash by convention.
        let verify_url = match self.rotation_url(index as u64, "") {
            Some(u) => u.trim_end_matches('/').to_string(),
            None => self.config.base_url.clone(),
        };

        let mut last_err = None;
        for _attempt in 0..=Self::INLINE_VERIFY_RETRIES {
            match verifier.create_verified_client(&verify_url).await {
                Ok(client) => {
                    let mut guard = self.index_clients[index]
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(ref existing) = *guard {
                        return Ok(existing.clone());
                    }
                    *guard = Some(client.clone());
                    return Ok(client);
                }
                Err(e) => {
                    let guard = self.index_clients[index]
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(ref existing) = *guard {
                        return Ok(existing.clone());
                    }
                    drop(guard);
                    tracing::warn!(index, error = %e, "Inline backend verification failed, retrying");
                    last_err = Some(e);
                }
            }
        }

        // Retries exhausted. Fall back to the non-pinned client ONLY if a
        // fingerprint is already pinned (its verifier still rejects unknown
        // SPKIs); in Bootstrap, fail closed to avoid unauthenticated connections.
        let err_msg = format!(
            "Inline backend verification failed after {} attempts: {}",
            Self::INLINE_VERIFY_RETRIES + 1,
            last_err.unwrap_or_default()
        );
        if self.pinned_fingerprint_count() > 0 {
            tracing::warn!(index, error = %err_msg, "Inline backend verification exhausted retries; serving with fallback client");
            Ok(self.fallback_client.clone())
        } else {
            tracing::warn!(
                index,
                error = %err_msg,
                "Inline backend verification exhausted retries in Bootstrap state; \
                 refusing fallback to prevent unauthenticated connections"
            );
            Err(CompletionError::CompletionError(err_msg))
        }
    }
}

impl Provider {
    /// Create a new vLLM provider with the given configuration.
    /// Without a `BackendVerifier`, per-index clients are pre-created eagerly
    /// (legacy behavior for tests and non-TEE environments).
    pub fn new(config: Config) -> Self {
        let fingerprint_state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
        Self::new_with_fingerprint_state(config, fingerprint_state)
    }

    /// Create a new vLLM provider sharing an existing fingerprint state.
    /// Without a `BackendVerifier`, per-index clients are pre-created eagerly.
    pub fn new_with_fingerprint_state(
        config: Config,
        fingerprint_state: Arc<std::sync::RwLock<FingerprintState>>,
    ) -> Self {
        Self::build(
            config,
            fingerprint_state,
            None,
            Self::inline_verify_concurrency_from_env(),
        )
    }

    /// Create a new vLLM provider with inline backend verification.
    /// Per-index clients are created lazily: on first use, the verifier connects
    /// to the index's backend, verifies attestation, pins the fingerprint, and
    /// returns a client whose H2 connection is pinned to that verified backend.
    pub fn new_with_verifier(
        config: Config,
        fingerprint_state: Arc<std::sync::RwLock<FingerprintState>>,
        verifier: Arc<dyn crate::BackendVerifier>,
    ) -> Self {
        Self::build(
            config,
            fingerprint_state,
            Some(verifier),
            Self::inline_verify_concurrency_from_env(),
        )
    }

    /// Test-only constructor that accepts an explicit `inline_verify_concurrency`
    /// so tests can exercise the semaphore logic without mutating env vars.
    #[cfg(test)]
    fn new_with_verifier_and_concurrency(
        config: Config,
        fingerprint_state: Arc<std::sync::RwLock<FingerprintState>>,
        verifier: Arc<dyn crate::BackendVerifier>,
        inline_verify_concurrency: usize,
    ) -> Self {
        Self::build(
            config,
            fingerprint_state,
            Some(verifier),
            inline_verify_concurrency,
        )
    }

    /// Read `INLINE_VERIFY_CONCURRENCY` from the environment, falling back to 4.
    fn inline_verify_concurrency_from_env() -> usize {
        std::env::var("INLINE_VERIFY_CONCURRENCY")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(4)
            .max(1)
    }

    fn build(
        config: Config,
        fingerprint_state: Arc<std::sync::RwLock<FingerprintState>>,
        backend_verifier: Option<Arc<dyn crate::BackendVerifier>>,
        inline_verify_concurrency: usize,
    ) -> Self {
        let tls_roots = SharedTlsRoots::load();

        // reqwest's read_timeout is a per-chunk idle timeout. For non-streaming
        // chat completion the connection is silent the entire inference time
        // (server computes, then sends the body in one shot) — so read_timeout
        // must be ≥ completion_timeout or it fires first and bypasses our
        // configured per-request budget.
        let completion_timeout = config.completion_timeout();
        let control_timeout = config.control_timeout();

        // General-purpose client for non-completion requests
        let client = Client::builder()
            .use_preconfigured_tls(tls_roots.build_config(fingerprint_state.clone()))
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(90))
            .read_timeout(control_timeout)
            .build()
            .expect("Failed to create HTTP client");

        // Fallback client: like the general client but with completion-timeout
        // read settings, so it can be used for long-running inference requests
        // when inline bucket verification fails.
        let fallback_client = Client::builder()
            .use_preconfigured_tls(tls_roots.build_config(fingerprint_state.clone()))
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(90))
            .read_timeout(completion_timeout)
            .build()
            .expect("Failed to create fallback HTTP client");

        let inline_verify_concurrency = inline_verify_concurrency.max(1);
        let verification_semaphore = Arc::new(Semaphore::new(inline_verify_concurrency));

        let prefix_router = Arc::new(PrefixRouter::new());

        // Per-index clients: one slot per rotation index, sized to the hard
        // fan-out cap. Lazily filled when a verifier is available (each index
        // gets a verified client pinned to backend `i` on first use), or
        // eagerly pre-created (legacy / non-TEE mode).
        let index_clients: Vec<std::sync::Mutex<Option<Client>>> = if backend_verifier.is_some() {
            (0..crate::rotation::MAX_FANOUT)
                .map(|_| std::sync::Mutex::new(None))
                .collect()
        } else {
            (0..crate::rotation::MAX_FANOUT)
                .map(|_| {
                    let builder = Client::builder()
                        .use_preconfigured_tls(tls_roots.build_config(fingerprint_state.clone()))
                        .pool_max_idle_per_host(1)
                        .http2_adaptive_window(true)
                        .connect_timeout(Duration::from_secs(5))
                        .read_timeout(completion_timeout);
                    // Index clients need the H2 connection to stay sticky to a
                    // single backend across long idle gaps; see
                    // `crate::bucket_keepalive`.
                    let c = crate::bucket_keepalive::apply(builder)
                        .build()
                        .expect("Failed to create index HTTP client");
                    std::sync::Mutex::new(Some(c))
                })
                .collect()
        };

        // Pre-parse the base URL into rotation parts once. URLs that don't fit
        // the rotation scheme (one-label host, IP literal, etc.) yield `None`,
        // disabling rotation fallback for that provider — the canonical-SNI
        // attempt's error simply propagates as it did before.
        let rotation_parts = url::Url::parse(&config.base_url)
            .ok()
            .as_ref()
            .and_then(crate::rotation::split_inference_url);

        Self {
            fleet: Arc::new(Fleet::new(
                rotation_parts,
                prefix_router,
                index_clients,
                config,
                client,
                fallback_client,
                verification_semaphore,
                fingerprint_state,
                backend_verifier,
            )),
        }
    }

    /// Access the provider's configuration.
    pub fn config(&self) -> &Config {
        &self.fleet.config
    }

    /// Get a reference to the shared fingerprint state.
    pub fn fingerprint_state(&self) -> Arc<std::sync::RwLock<FingerprintState>> {
        self.fleet.fingerprint_state.clone()
    }

    /// Add a verified SPKI fingerprint. Transitions Bootstrap → Pinned,
    /// or adds to existing Pinned set. Unblocks a Blocked provider.
    pub fn add_verified_fingerprint(&self, fingerprint: String) {
        self.fleet
            .fingerprint_state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .add_fingerprint(fingerprint);
    }

    /// Block all TLS connections (attestation verification failed).
    /// Only blocks from Bootstrap state — does not override existing Pinned fingerprints.
    pub fn block_connections(&self) {
        self.fleet.block_connections();
    }

    /// Returns the number of verified fingerprints currently pinned.
    pub fn pinned_fingerprint_count(&self) -> usize {
        self.fleet.pinned_fingerprint_count()
    }

    /// Spawn background tasks to pre-warm the live per-index clients (delegates
    /// to [`Fleet::pre_warm`]; no-op without a verifier or before any
    /// fingerprint is pinned).
    pub fn pre_warm(self: Arc<Self>) {
        self.fleet.clone().pre_warm();
    }
}

/// Network/IO helpers (rotation-SNI fallback + request header prep), owned by
/// Fleet. Moved off Provider in step 4c.
impl Fleet {
    /// Prepare encryption headers by extracting them from `extra` and forwarding as HTTP headers.
    /// Also removes encryption-related keys from `extra` to prevent them from leaking into the JSON body.
    ///
    /// NOTE: `x_model_pub_key` is intentionally not forwarded to vllm-proxy. It is consumed by the
    /// cloud API layer for provider routing and is not needed by the downstream vllm-proxy, so it
    /// is stripped from `extra` without being added as an HTTP header.
    fn prepare_encryption_headers(
        &self,
        headers: &mut reqwest::header::HeaderMap,
        extra: &mut std::collections::HashMap<String, serde_json::Value>,
    ) {
        // Extract and forward x_signing_algo as HTTP header, then remove from extra
        if let Some(algo) = extra
            .remove(encryption_headers::SIGNING_ALGO)
            .as_ref()
            .and_then(|v| v.as_str())
        {
            if let Ok(value) = HeaderValue::from_str(algo) {
                headers.insert("X-Signing-Algo", value);
            }
        }

        // Extract and forward x_client_pub_key as HTTP header, then remove from extra
        if let Some(pub_key) = extra
            .remove(encryption_headers::CLIENT_PUB_KEY)
            .as_ref()
            .and_then(|v| v.as_str())
        {
            if let Ok(value) = HeaderValue::from_str(pub_key) {
                headers.insert("X-Client-Pub-Key", value);
            }
        }

        // Remove x_model_pub_key from extra (not forwarded to vllm-proxy, used only for routing)
        extra.remove(encryption_headers::MODEL_PUB_KEY);

        // Extract and forward x_encryption_version as HTTP header, then remove from extra
        if let Some(version) = extra
            .remove(encryption_headers::ENCRYPTION_VERSION)
            .as_ref()
            .and_then(|v| v.as_str())
        {
            if let Ok(value) = HeaderValue::from_str(version) {
                headers.insert("X-Encryption-Version", value);
            }
        }

        // Extract and forward x_encrypt_all_fields as HTTP header, then remove from extra
        if let Some(val) = extra
            .remove(encryption_headers::ENCRYPT_ALL_FIELDS)
            .as_ref()
            .and_then(|v| v.as_str())
        {
            if let Ok(value) = HeaderValue::from_str(val) {
                headers.insert("X-Encrypt-All-Fields", value);
            }
        }
    }

    /// Prepare tracing headers by extracting correlation IDs from `extra` and forwarding
    /// as HTTP headers. Removes the keys from `extra` so they don't leak into the JSON body.
    ///
    /// Silently skips any key whose value is not a valid ASCII header value.
    /// Independent of `prepare_encryption_headers` — call order does not matter.
    fn prepare_tracing_headers(
        &self,
        headers: &mut reqwest::header::HeaderMap,
        extra: &mut std::collections::HashMap<String, serde_json::Value>,
    ) {
        // X-Request-Id — join key across all hops
        if let Some(id) = extra
            .remove(tracing_headers::REQUEST_ID)
            .as_ref()
            .and_then(|v| v.as_str())
        {
            if let Ok(value) = HeaderValue::from_str(id) {
                headers.insert("X-Request-Id", value);
            }
        }

        // X-Org-Id — organisation that owns the API key
        if let Some(org) = extra
            .remove(tracing_headers::ORG_ID)
            .as_ref()
            .and_then(|v| v.as_str())
        {
            if let Ok(value) = HeaderValue::from_str(org) {
                headers.insert("X-Org-Id", value);
            }
        }

        // X-Workspace-Id — workspace of the API key
        if let Some(ws) = extra
            .remove(tracing_headers::WORKSPACE_ID)
            .as_ref()
            .and_then(|v| v.as_str())
        {
            if let Ok(value) = HeaderValue::from_str(ws) {
                headers.insert("X-Workspace-Id", value);
            }
        }
    }

    /// Send a streaming HTTP POST request with TTFB timeout protection.
    ///
    /// Uses `tokio::time::timeout` only around `.send()` so the timeout applies to TTFB only
    /// (connect + response headers), not to body consumption. reqwest's `.timeout()` on the
    /// `RequestBuilder` applies to the full request lifecycle including body streaming, which
    /// kills long-running SSE streams at 30s.
    ///
    /// `client_override` allows using a dedicated client for connection pinning.
    async fn send_streaming_request<T: serde::Serialize + Send + Sync>(
        &self,
        url: &str,
        headers: reqwest::header::HeaderMap,
        params: &T,
        client_override: Option<&Client>,
    ) -> Result<reqwest::Response, CompletionError> {
        let client = client_override.unwrap_or(&self.client);
        let ttfb_timeout_secs = self.config.control_timeout_seconds.max(0) as u64;
        let response = tokio::time::timeout(
            self.config.control_timeout(),
            client.post(url).headers(headers).json(params).send(),
        )
        .await
        // TTFB stalls indicate the same backend is stuck — surface as
        // `Timeout` (non-retryable in the pool) for consistency with the
        // non-streaming path. Pre-`Timeout` this was an `HttpError 504` and
        // got retried up to 4× by the pool, burning 4 × control_timeout for
        // no gain. We still don't surface fingerprint mismatches as Timeout
        // — those land in the second `?` arm below.
        .map_err(|_| CompletionError::Timeout {
            operation: "chat_completion_stream".to_string(),
            timeout_seconds: ttfb_timeout_secs,
        })?
        .map_err(|e| CompletionError::CompletionError(format_error_chain(&e)))?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
            return Err(CompletionError::HttpError {
                status_code,
                message: crate::extract_error_message(&error_text),
                is_external: false,
            });
        }

        Ok(response)
    }

    /// Status codes that warrant a rotation-SNI retry. Mirrors the pool's
    /// `classify_retry_decision` ("retryable_http_5xx" + 429 + 408), but
    /// evaluated here so the rotation fallback fires *before* the canonical
    /// 5xx escapes to the pool's same-provider backoff loop (which would
    /// only re-hit the sticky bucket → same overloaded backend). 408 is
    /// included because the pool already treats it as "next-provider-
    /// worthy" — keeping the gates in sync avoids a taxonomy mismatch
    /// where the pool would retry on 408 but rotation wouldn't.
    fn is_rotation_retryable_status(status_code: u16) -> bool {
        status_code == 408 || status_code == 429 || (500..=599).contains(&status_code)
    }

    /// Walk the fallback indices (ordered fastest-EMA first) until one returns
    /// a 2xx (or every backend has been exhausted). Called by `chat_completion`
    /// after the sticky index's attempt returns 5xx/429. Each index uses its
    /// pooled, attestation-verified client (`get_or_verify_index_client`), so a
    /// served completion is always from a verified backend, and the served
    /// index is recorded in `signature_rotation` so the signature fetch reuses
    /// the same warm connection.
    ///
    /// `canonical_err` is the error that triggered the fallback; if all
    /// indices return retryable failures it surfaces as the final error to
    /// preserve the original `status_code` for `map_provider_error`.
    async fn try_chat_completion_fallback_indices(
        &self,
        indices: &[usize],
        params: &ChatCompletionParams,
        headers: &reqwest::header::HeaderMap,
        timeout: Duration,
        canonical_err: CompletionError,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        // `last_error` tracks only HttpError-shaped failures so the pool's
        // `classify_retry_decision` sees a typed `retryable_http_5xx` (or
        // 429) at the end. Transport-level failures from the fallback loop
        // (Timeout, generic CompletionError) are logged but never overwrite
        // `last_error`: if every index produced only transport
        // errors, we fall back to `canonical_err` (always an HttpError 5xx/
        // 429 by call-site construction) instead of returning a misleading
        // `CompletionError(...)` that would classify as
        // `retryable_connection_keyword`.
        let mut last_error = canonical_err;
        for &index in indices {
            let url = match self.rotation_url(index as u64, "/v1/chat/completions") {
                Some(u) => u,
                None => continue,
            };
            let client = match self.get_or_verify_index_client(index).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(
                        index, error = %e,
                        "Fallback index client unavailable, trying next backend"
                    );
                    continue;
                }
            };
            let send_res = client
                .post(&url)
                .headers(headers.clone())
                .json(params)
                .timeout(timeout)
                .send()
                .await;
            let response = match send_res {
                Ok(r) => r,
                Err(e) => {
                    // Connect / network / TTFB-timeout errors against this
                    // index — try the next backend. The rotation listener
                    // pins to one backend by design (model-proxy PR #27),
                    // so failure at index N tells us nothing about N+1.
                    // We log the error but DON'T overwrite `last_error`:
                    // see the field comment above.
                    tracing::debug!(
                        index, error = %format_error_chain(&e),
                        is_timeout = e.is_timeout(),
                        is_connect = e.is_connect(),
                        "Fallback index chat_completion attempt errored, trying next backend"
                    );
                    continue;
                }
            };
            if !response.status().is_success() {
                let status_code = response.status().as_u16();
                let error_text = response
                    .text()
                    .await
                    .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
                let err = CompletionError::HttpError {
                    status_code,
                    message: crate::extract_error_message(&error_text),
                    is_external: false,
                };
                if Fleet::is_rotation_retryable_status(status_code) {
                    tracing::debug!(
                        index,
                        status_code,
                        "Fallback index chat_completion backend still 5xx/429/408, trying next"
                    );
                    last_error = err;
                    continue;
                }
                // 4xx (other than 408/429) means the request itself is bad —
                // surface immediately rather than burn the rest of the
                // fallback set on the same client error.
                return Err(err);
            }

            let raw_bytes = response
                .bytes()
                .await
                .map_err(|e| CompletionError::CompletionError(format_error_chain(&e)))?
                .to_vec();
            let chat_completion_response: ChatCompletionResponse =
                serde_json::from_slice(&raw_bytes).map_err(|e| {
                    CompletionError::CompletionError(format!("Failed to parse response: {e}"))
                })?;

            let chat_id = chat_completion_response.id.clone();
            self.signature_rotation
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(chat_id, index as u64);
            tracing::info!(
                index,
                "Fallback index chat_completion served by alternative backend"
            );
            return Ok(ChatCompletionResponseWithBytes {
                response: chat_completion_response,
                raw_bytes,
                serving_tier: crate::ProviderTier::Near,
            });
        }
        Err(last_error)
    }

    /// Streaming sibling of `try_chat_completion_fallback_indices`. Two failure
    /// modes land here:
    ///   - The sticky index's send returned HTTP 5xx/429 outright (e.g. nginx
    ///     502 when the inference-proxy container is restarting).
    ///   - The sticky index's send returned HTTP 200 but the first SSE chunk was
    ///     a `{"error":{"code":...}}` frame, which the parser now surfaces as
    ///     a typed `HttpError` (the SGLang queue-full path that inference-
    ///     proxy's SseTransformer forwards verbatim).
    ///
    /// We walk `indices` (ordered fastest-EMA first by the caller) until one
    /// returns a 200 whose first SSE chunk is a real content event. Each index
    /// uses its pooled, attestation-verified client, so a served stream is
    /// always from a verified backend; the served index is recorded in
    /// `pending_rotation` and the returned stream is wrapped in a TTFT probe.
    /// Bytes already sent to the client are zero (peek happens before any chunk
    /// forwarding), so retrying the whole stream is safe.
    async fn try_stream_fallback_indices(
        &self,
        indices: &[usize],
        params: &ChatCompletionParams,
        headers: &reqwest::header::HeaderMap,
        request_hash: &str,
        canonical_err: CompletionError,
    ) -> Result<StreamingResult, CompletionError> {
        // See the non-streaming sibling for the design: `last_error` only
        // tracks HttpError-shaped failures from fallback indices, so the
        // pool's `classify_retry_decision` sees the right `retryable_http_*`
        // label at the end. Transport-level failures (Timeout, generic
        // CompletionError) are logged but never overwrite `last_error`;
        // if the fallback produces only transport errors, we fall back to
        // `canonical_err` (always HttpError 5xx/429 by call-site
        // construction).
        let mut last_error = canonical_err;
        for &index in indices {
            let url = match self.rotation_url(index as u64, "/v1/chat/completions") {
                Some(u) => u,
                None => continue,
            };
            let client = match self.get_or_verify_index_client(index).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(
                        index, error = %e,
                        "Fallback index stream client unavailable, trying next backend"
                    );
                    continue;
                }
            };
            // Capture the send instant for the per-backend TTFT measurement.
            let started = std::time::Instant::now();
            let response = match self
                .send_streaming_request(&url, headers.clone(), params, Some(&client))
                .await
            {
                Ok(r) => r,
                Err(e) => match &e {
                    // 4xx other than 408/429 is a real client error (bad
                    // request, invalid params) — every backend would reject
                    // it the same way, so surface immediately rather than
                    // burn the remaining indices on a doomed request.
                    CompletionError::HttpError { status_code, .. }
                        if !Fleet::is_rotation_retryable_status(*status_code) =>
                    {
                        return Err(e);
                    }
                    // Retryable HttpError (5xx/429/408) — update last_error
                    // so the trace label stays accurate at end-of-fallback.
                    CompletionError::HttpError { .. } => {
                        tracing::debug!(
                            index, error = %e,
                            "Fallback index stream attempt returned 5xx/429/408, trying next backend"
                        );
                        last_error = e;
                        continue;
                    }
                    // Transport-level failures (`Timeout` from
                    // `send_streaming_request`'s TTFB guard, generic
                    // `CompletionError` for TLS/TCP). Per-backend by
                    // construction: model-proxy PR #27 pins each `-iN` SNI
                    // to one backend, so failure at index N tells us nothing
                    // about index N+1. Log but DON'T overwrite last_error.
                    _ => {
                        tracing::debug!(
                            index,
                            error = %e,
                            "Fallback index stream attempt failed transport, trying next backend"
                        );
                        continue;
                    }
                },
            };
            let parser = new_sse_parser(response.bytes_stream(), true);
            let stream: StreamingResult = Box::pin(parser);
            let (first_chunk_status, stream) = Self::peek_first_payload_status(stream).await;
            if let Some(status_code) = first_chunk_status {
                tracing::debug!(
                    index,
                    status_code,
                    "Fallback index stream attempt: first chunk was an error, trying next backend"
                );
                last_error = CompletionError::HttpError {
                    status_code,
                    message: "Upstream stream emitted an error event".to_string(),
                    is_external: false,
                };
                drop(stream);
                continue;
            }
            self.pending_rotation
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(request_hash.to_string(), index as u64);
            tracing::info!(
                index,
                "Fallback index chat_completion_stream served by alternative backend"
            );
            let probed: StreamingResult = Box::pin(TtftProbe::new(
                stream,
                self.backend_stats.clone(),
                index,
                started,
            ));
            return Ok(probed);
        }
        Err(last_error)
    }

    /// Peek past any leading control events (chunk-less `SSEEvent`s — e.g. a
    /// keepalive comment or blank line surfaced by the lossless passthrough
    /// parser, issue #701) to classify the first real SSE payload. Returns
    /// the upstream status code when that first payload is an in-stream
    /// `HttpError` eligible for rotation fallback, together with the stream
    /// with all consumed control events re-attached in order (they are part
    /// of the signed byte stream and must still reach the client). Without
    /// the skip, a leading control event would mask a first-chunk error frame
    /// (e.g. SGLang queue-full) and bypass rotation.
    async fn peek_first_payload_status(stream: StreamingResult) -> (Option<u16>, StreamingResult) {
        // Cap the control-event skip so a keepalive-only upstream can't stall
        // first-chunk classification or grow the stash unbounded; past the cap
        // we stop skipping and classify whatever we've reached.
        const MAX_LEADING_CONTROL_EVENTS: usize = 32;
        let mut peekable = StreamingResultExt::peekable(stream);
        let mut leading_control: Vec<Result<SSEEvent, CompletionError>> = Vec::new();
        while leading_control.len() < MAX_LEADING_CONTROL_EVENTS
            && matches!(peekable.peek().await, Some(Ok(event)) if event.chunk.is_none())
        {
            if let Some(ev) = tokio_stream::StreamExt::next(&mut peekable).await {
                leading_control.push(ev);
            }
        }

        let status = if let Some(Err(CompletionError::HttpError { status_code, .. })) =
            peekable.peek().await
        {
            if Fleet::is_rotation_retryable_status(*status_code) {
                Some(*status_code)
            } else {
                None
            }
        } else {
            None
        };

        let stream: StreamingResult = if leading_control.is_empty() {
            Box::pin(peekable)
        } else {
            Box::pin(futures_util::StreamExt::chain(
                futures_util::stream::iter(leading_control),
                peekable,
            ))
        };
        (status, stream)
    }
}

/// Stream adapter that measures time-to-first-content-token for a backend
/// index and folds it into the per-index TTFT EMA.
///
/// It holds only a clone of the `backend_stats` Arc (not `&Fleet`), so the
/// measurement happens lazily as the client consumes the stream — after the
/// originating Fleet method has returned. The probe records exactly once, on
/// the first SSE event that carries parsed content (`event.chunk.is_some()`),
/// then becomes a transparent pass-through. No request/response content is
/// touched or logged — only a latency number bound to an index.
struct TtftProbe<S> {
    inner: S,
    stats: Arc<std::sync::Mutex<Vec<fleet::BackendStat>>>,
    index: usize,
    /// Send instant; `None` once the first content TTFT has been recorded.
    start: Option<std::time::Instant>,
}

impl<S> TtftProbe<S> {
    fn new(
        inner: S,
        stats: Arc<std::sync::Mutex<Vec<fleet::BackendStat>>>,
        index: usize,
        start: std::time::Instant,
    ) -> Self {
        Self {
            inner,
            stats,
            index,
            start: Some(start),
        }
    }
}

impl<S> futures_util::Stream for TtftProbe<S>
where
    S: futures_util::Stream<Item = Result<SSEEvent, CompletionError>> + Unpin,
{
    type Item = Result<SSEEvent, CompletionError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let polled = std::pin::Pin::new(&mut self.inner).poll_next(cx);
        if let std::task::Poll::Ready(Some(Ok(ref event))) = polled {
            // Only a content-bearing chunk counts as the first token; leading
            // control events (keepalives, blank separators) don't.
            if event.chunk.is_some() {
                if let Some(start) = self.start.take() {
                    let ttft_ms = start.elapsed().as_millis() as f64;
                    let index = self.index;
                    let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(s) = stats.get_mut(index) {
                        fleet::update_ema(s, ttft_ms);
                    }
                }
            }
        }
        polled
    }
}

#[async_trait]
impl InferenceProvider for Fleet {
    /// NEAR's own attested fleet. `Provider` (which wraps `Fleet`) is what the pool
    /// actually registers, but mirror the tier here too so the verifiable filter can
    /// never misclassify a `Fleet` as plaintext if one is ever pooled directly.
    fn tier(&self) -> crate::ProviderTier {
        crate::ProviderTier::Near
    }

    fn provider_source(&self) -> crate::ProviderSource {
        crate::ProviderSource::Vllm
    }

    async fn get_signature(
        &self,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<ChatSignature, CompletionError> {
        let signing_algo = signing_algo.unwrap_or_else(|| "ecdsa".to_string());
        let path_and_query = format!("/v1/signature/{chat_id}?signing_algo={signing_algo}");
        let canonical_url = format!("{}{}", self.config.base_url, path_and_query);
        let headers = self
            .build_headers()
            .map_err(CompletionError::CompletionError)?;
        let timeout = self.config.control_timeout();

        // Resolve the backend-index target once; it's stable across retries.
        // Every index-routed completion records `signature_rotation[chat_id] =
        // index`, so the signature lives on that *specific* backend. We reuse
        // the warm, pooled, attestation-verified index client — the same
        // connection that served the completion — to fetch it. Only chats with
        // no pin (the count==0 canonical path) fall back to the general LB
        // client.
        let rotation_index = self
            .signature_rotation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(chat_id)
            .copied();
        let rotation_target = if let Some(index) = rotation_index {
            match self.rotation_url(index, "") {
                Some(base) => {
                    let url = format!("{}{}", base.trim_end_matches('/'), path_and_query);
                    // Reuse the pooled, verified index client (same warm
                    // connection as the completion). Lazy-create + verify it if
                    // it was cleared (e.g. a count change reset the slot).
                    match self.get_or_verify_index_client(index as usize).await {
                        Ok(client) => Some((index, url, client)),
                        Err(e) => {
                            tracing::debug!(index, error = %e, "Signature index client unavailable");
                            None
                        }
                    }
                }
                None => None,
            }
        } else {
            None
        };

        // No pin → walk the general-purpose LB client (count==0 canonical path).
        let clients_to_try: Vec<&Client> = vec![&self.client];

        // The backend signs in a background task that finalizes *after* the
        // final stream chunk, then caches the signature. cloud-api fetches in
        // the hot path the instant the stream ends, so a 404 on every reachable
        // backend is usually a signing race (the signature lands a few ms
        // later), not a permanent miss. Retry the whole fetch with a short,
        // bounded backoff. Only a 404 is retried: a non-404 status is
        // definitive, and a transport error is definitive on the pinned index
        // backend or once it reaches the general client.
        //
        // Latency bound: the backoffs add at most their sum (~350ms), but each
        // request also keeps its own `control_timeout`, so a slow (not fast-404)
        // backend can still take longer per attempt. The overall fetch is
        // ultimately bounded by the caller's hot-path FINALIZE_TIMEOUT, which
        // cancels the whole store if it runs long.
        let mut last_error = None;
        for attempt in 0..=SIGNATURE_FETCH_BACKOFFS_MS.len() {
            // For an index-pinned chat the signature was produced on that
            // *specific* backend, so its response is the ONLY authoritative one:
            // the general client can't have the signature, and probing it risks
            // a non-authoritative 5xx/transport error suppressing a retryable
            // index 404 (the signature would then be lost). So when an index pin
            // exists we talk only to it — a 404 is the signing-race signal
            // (retry), and any non-404 status or transport error (e.g. a TLS
            // SPKI/attestation mismatch) is definitive and fails fast. Without a
            // pin we walk the general client and treat an all-404 sweep as the
            // race signal.
            let retryable;
            if let Some((index, rotation_url, client)) = &rotation_target {
                let index = *index;
                match client
                    .get(rotation_url.as_str())
                    .headers(headers.clone())
                    .timeout(timeout)
                    .send()
                    .await
                {
                    Ok(response) if response.status().is_success() => {
                        return response
                            .json()
                            .await
                            .map_err(|e| CompletionError::CompletionError(format_error_chain(&e)));
                    }
                    Ok(response) => {
                        let status = response.status().as_u16();
                        let error_text = response
                            .text()
                            .await
                            .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
                        last_error = Some(format!(
                            "Rotation-SNI signature fetch failed (HTTP {status}): {error_text}"
                        ));
                        // 404 == signing race on the authoritative backend.
                        retryable = status == 404;
                        tracing::debug!(
                            index,
                            status,
                            "Rotation-SNI signature fetch did not return 2xx"
                        );
                    }
                    Err(e) => {
                        let message = format_error_chain(&e);
                        last_error = Some(format!(
                            "Rotation-SNI signature fetch transport error: {message}"
                        ));
                        retryable = false;
                        tracing::debug!(index, error = %message, "Rotation-SNI signature fetch errored");
                    }
                }
            } else {
                // Bucket client, then general LB client.
                let client_count = clients_to_try.len();
                let mut all_clients_404 = false;
                for (idx, &client) in clients_to_try.iter().enumerate() {
                    let response = match client
                        .get(&canonical_url)
                        .headers(headers.clone())
                        .timeout(timeout)
                        .send()
                        .await
                    {
                        Ok(response) => response,
                        // A transport error on a non-final client (e.g. a stale
                        // bucket connection to a dead backend) shouldn't abort
                        // the whole fetch — fall through to the next client.
                        // Only the last client's transport error is fatal, and
                        // transport errors never arm the 404 retry.
                        Err(e) => {
                            let message = format_error_chain(&e);
                            if idx + 1 < client_count {
                                tracing::debug!(
                                    error = %message,
                                    "Signature fetch transport error; trying next client"
                                );
                                last_error = Some(message);
                                continue;
                            }
                            return Err(CompletionError::CompletionError(message));
                        }
                    };

                    if response.status().is_success() {
                        let signature = response.json().await.map_err(|e| {
                            CompletionError::CompletionError(format_error_chain(&e))
                        })?;
                        return Ok(signature);
                    }

                    let status = response.status().as_u16();
                    let error_text = response
                        .text()
                        .await
                        .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
                    last_error = Some(format!(
                        "Signature fetch failed (HTTP {status}): {error_text}"
                    ));

                    // 404 == signature not present on this backend; try the next
                    // client. Any other status is definitive.
                    if status == 404 {
                        all_clients_404 = true;
                    } else {
                        all_clients_404 = false;
                        break;
                    }
                }
                retryable = all_clients_404;
            }

            // A 404 (signing race) is the only retryable outcome. Back off and
            // re-fetch — unless retries are exhausted or the failure was
            // definitive.
            if retryable {
                if let Some(backoff) = signature_fetch_backoff(attempt) {
                    tracing::debug!(
                        %chat_id,
                        attempt = attempt + 1,
                        "Signature not yet present on backend (404); retrying after backoff"
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
            }
            break;
        }

        Err(CompletionError::CompletionError(
            last_error.unwrap_or_else(|| "Signature fetch failed".to_string()),
        ))
    }

    fn pin_chat_connection(&self, request_hash: &str, chat_id: &str) {
        self.pin_chat(request_hash, chat_id);
    }

    fn unpin_chat_connection(&self, chat_id: &str) {
        self.unpin_chat(chat_id);
    }

    fn set_backend_count(&self, count: usize) {
        self.store_backend_count(count);
    }

    async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
    ) -> Result<serde_json::Map<String, serde_json::Value>, AttestationError> {
        #[derive(Serialize)]
        struct Query {
            model: String,
            signing_algo: Option<String>,
            nonce: Option<String>,
            signing_address: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            include_tls_fingerprint: Option<bool>,
        }

        let query = Query {
            model,
            signing_algo,
            nonce,
            signing_address,
            include_tls_fingerprint: include_tls_fingerprint.then_some(true),
        };

        // Build URL with optional query parameters
        let url = format!(
            "{}/v1/attestation/report?{}",
            self.config.base_url,
            serde_urlencoded::to_string(&query).map_err(|_| AttestationError::Unknown(
                "Failed to serialize query string".to_string()
            ))?
        );

        let headers = self.build_headers().map_err(AttestationError::FetchError)?;

        let response = self
            .client
            .get(&url)
            .headers(headers)
            .timeout(self.config.control_timeout())
            .send()
            .await
            .map_err(|e| AttestationError::FetchError(e.to_string()))?;

        // Handle 404 responses (expected when signing_address doesn't match)
        if response.status() == 404 {
            return Err(AttestationError::SigningAddressNotFound(
                query.signing_address.unwrap_or_default().to_string(),
            ));
        }

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
            return Err(AttestationError::FetchError(format!(
                "HTTP {status}: {error_text}",
            )));
        }

        let attestation_report = response
            .json()
            .await
            .map_err(|e| AttestationError::InvalidResponse(e.to_string()))?;
        Ok(attestation_report)
    }

    /// Lists all available models from the vLLM server
    async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        let canonical_url = format!("{}/v1/models", self.config.base_url);
        tracing::debug!("Listing models from vLLM server, url: {}", canonical_url);

        let headers = self.build_headers().map_err(ListModelsError::FetchError)?;
        let timeout = self.config.control_timeout();
        let client = self.client.clone();
        let rotation_count = self.rotation_count();
        let mut requests = Vec::with_capacity(rotation_count + 1);
        requests.push((
            canonical_url.clone(),
            ModelsRequest {
                client: client.clone(),
                headers: headers.clone(),
                timeout,
                url: canonical_url,
            },
        ));

        requests.extend((0..rotation_count).filter_map(|index| {
            self.rotation_url(index as u64, "/v1/models").map(|url| {
                (
                    url.clone(),
                    ModelsRequest {
                        client: client.clone(),
                        headers: headers.clone(),
                        timeout,
                        url,
                    },
                )
            })
        }));

        let results = futures_util::future::join_all(
            requests
                .into_iter()
                .map(|(url, request)| async move { (url, send_models_request(request).await) }),
        )
        .await;

        let mut responses = Vec::with_capacity(results.len());
        let mut first_error = None;
        for (url, result) in results {
            match result {
                Ok(response) => responses.push(response),
                Err(error) => {
                    tracing::warn!(
                        url = %url,
                        error = %error,
                        "Failed to list models from backend"
                    );
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        if responses.is_empty() {
            return Err(first_error.unwrap_or(ListModelsError::Unknown));
        }

        Ok(merge_model_responses(responses))
    }

    /// Performs a streaming chat completion request
    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        // Ensure streaming and token usage are enabled
        let mut streaming_params = params;
        // #666: self-hosted vLLM serves a verbatim copy of `ChatMessage.content`,
        // so drop Anthropic prompt-caching breakpoints before routing and
        // serialization — vLLM may 400 on an unknown `cache_control` content-part
        // field (the breakpoint only matters on the Anthropic upstream).
        crate::strip_cache_control(&mut streaming_params.messages);
        streaming_params.stream = Some(true);
        streaming_params.stream_options = Some(StreamOptions {
            include_usage: Some(true),
            continuous_usage_stats: Some(true),
            extra: Default::default(),
        });

        let mut headers = self
            .build_headers()
            .map_err(CompletionError::CompletionError)?;
        let request_hash_value = HeaderValue::from_str(&request_hash)
            .map_err(|e| CompletionError::CompletionError(format!("Invalid request hash: {e}")))?;
        headers.insert("X-Request-Hash", request_hash_value);

        // Prepare tracing headers (request_id, org_id, workspace_id)
        self.prepare_tracing_headers(&mut headers, &mut streaming_params.extra);
        // Prepare encryption headers
        self.prepare_encryption_headers(&mut headers, &mut streaming_params.extra);

        // Select the backend rotation index: prefix affinity → same backend →
        // prefix cache hit, with latency steering off a pathologically slow
        // backend. `None` means rotation is unavailable (cold-start before
        // discovery's first count, or a non-rotation URL like `localhost`) —
        // serve via the canonical fallback path (no index, no per-backend
        // measurement) so those paths keep working unchanged.
        let index = match self.select_index(&streaming_params.messages) {
            None => {
                let url = format!("{}/v1/chat/completions", self.config.base_url);
                let response = self
                    .send_streaming_request(
                        &url,
                        headers.clone(),
                        &streaming_params,
                        Some(&self.fallback_client),
                    )
                    .await?;
                let sse_stream = new_sse_parser(response.bytes_stream(), true);
                return Ok(Box::pin(sse_stream));
            }
            Some(i) => i,
        };

        // The index client maintains a persistent H2 connection to a verified
        // backend (`<canonical>-i<index>.<base>`) via L4 passthrough → prefix
        // cache hits. Index clients are lazily filled: on first use, inline
        // verification connects to the index's backend, verifies attestation,
        // and pins the client.
        let url = self
            .rotation_url(index as u64, "/v1/chat/completions")
            .unwrap_or_else(|| format!("{}/v1/chat/completions", self.config.base_url));
        let index_client = self.get_or_verify_index_client(index).await?;
        // Capture the send instant for the per-backend TTFT measurement.
        let started = std::time::Instant::now();
        let primary_send = match self
            .send_streaming_request(
                &url,
                headers.clone(),
                &streaming_params,
                Some(&index_client),
            )
            .await
        {
            Ok(r) => Ok(r),
            Err(ref e) if Fleet::is_connection_error(e) => {
                // Connection dropped or fingerprint mismatch on reconnect —
                // clear the index client and re-verify with a fresh attestation.
                self.clear_index(index);
                let fresh = self.get_or_verify_index_client(index).await?;
                self.send_streaming_request(&url, headers.clone(), &streaming_params, Some(&fresh))
                    .await
            }
            Err(e) => Err(e),
        };

        // Decision tree before exposing the stream:
        //   - HTTP-level 5xx/429 (status arrived in response headers): walk the
        //     other backend indices ordered by EMA.
        //   - HTTP 200 + first SSE chunk is `{"error":{"code":N,...}}`
        //     (SGLang queue-full path, which inference-proxy's SseTransformer
        //     forwards verbatim): peek catches it via the parser's typed
        //     `HttpError` and we route to the same fallback.
        //   - Otherwise: record the index, wrap the stream in a TTFT probe, and
        //     return it as the live stream.
        //
        // Rotation is always possible on this path (we have an index), so we
        // always peek: the peek blocks until the first SSE chunk arrives, so on
        // the happy path it adds first-byte latency to the streaming request —
        // the cost of being able to reroute off a first-chunk error frame.
        match primary_send {
            Ok(response) => {
                let parser = new_sse_parser(response.bytes_stream(), true);
                let stream: StreamingResult = Box::pin(parser);
                let (first_chunk_status, stream) = Self::peek_first_payload_status(stream).await;
                match first_chunk_status {
                    None => {
                        self.pending_rotation
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(request_hash, index as u64);
                        let probed: StreamingResult = Box::pin(TtftProbe::new(
                            stream,
                            self.backend_stats.clone(),
                            index,
                            started,
                        ));
                        Ok(probed)
                    }
                    Some(status_code) => {
                        drop(stream);
                        self.try_stream_fallback_indices(
                            &self.fallback_indices(index),
                            &streaming_params,
                            &headers,
                            &request_hash,
                            CompletionError::HttpError {
                                status_code,
                                message: "Upstream stream emitted an error event".to_string(),
                                is_external: false,
                            },
                        )
                        .await
                    }
                }
            }
            Err(canonical_err) => match &canonical_err {
                CompletionError::HttpError { status_code, .. }
                    if Fleet::is_rotation_retryable_status(*status_code) =>
                {
                    self.try_stream_fallback_indices(
                        &self.fallback_indices(index),
                        &streaming_params,
                        &headers,
                        &request_hash,
                        canonical_err,
                    )
                    .await
                }
                _ => Err(canonical_err),
            },
        }
    }

    /// Performs a chat completion request
    async fn chat_completion(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        let mut non_streaming_params = params;
        // #666: drop Anthropic prompt-caching breakpoints before forwarding to
        // self-hosted vLLM (see the streaming path for the rationale).
        crate::strip_cache_control(&mut non_streaming_params.messages);

        let mut headers = self
            .build_headers()
            .map_err(CompletionError::CompletionError)?;
        let request_hash_value = HeaderValue::from_str(&request_hash)
            .map_err(|e| CompletionError::CompletionError(format!("Invalid request hash: {e}")))?;
        headers.insert("X-Request-Hash", request_hash_value);

        // Prepare tracing headers (request_id, org_id, workspace_id)
        self.prepare_tracing_headers(&mut headers, &mut non_streaming_params.extra);
        // Prepare encryption headers
        self.prepare_encryption_headers(&mut headers, &mut non_streaming_params.extra);

        let timeout_secs = self.config.completion_timeout_seconds.max(0) as u64;
        let timeout = Duration::from_secs(timeout_secs);

        // Distinguish timeout from other transport errors so the pool can refuse
        // to retry timeouts (a re-send hits the same model with the same prompt).
        // Connect-level timeouts are excluded: those usually indicate transient
        // network blips and are worth retrying via the index-clear path below.
        let map_send_err = |e: reqwest::Error| -> CompletionError {
            if e.is_timeout() && !e.is_connect() {
                CompletionError::Timeout {
                    operation: "chat_completion".to_string(),
                    timeout_seconds: timeout_secs,
                }
            } else {
                CompletionError::CompletionError(format_error_chain(&e))
            }
        };

        // Select the backend rotation index (prefix affinity + latency
        // steering). `None` → canonical fallback path (cold-start / non-rotation
        // URL): one shot via the non-pinned fallback client, no index recorded.
        let index = match self.select_index(&non_streaming_params.messages) {
            None => {
                let url = format!("{}/v1/chat/completions", self.config.base_url);
                let response = self
                    .fallback_client
                    .post(&url)
                    .headers(headers.clone())
                    .json(&non_streaming_params)
                    .timeout(timeout)
                    .send()
                    .await
                    .map_err(map_send_err)?;
                if !response.status().is_success() {
                    let status_code = response.status().as_u16();
                    let error_text = response
                        .text()
                        .await
                        .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
                    return Err(CompletionError::HttpError {
                        status_code,
                        message: crate::extract_error_message(&error_text),
                        is_external: false,
                    });
                }
                let raw_bytes = response.bytes().await.map_err(map_send_err)?.to_vec();
                let chat_completion_response: ChatCompletionResponse =
                    serde_json::from_slice(&raw_bytes).map_err(|e| {
                        CompletionError::CompletionError(format!("Failed to parse response: {e}"))
                    })?;
                return Ok(ChatCompletionResponseWithBytes {
                    response: chat_completion_response,
                    raw_bytes,
                    serving_tier: crate::ProviderTier::Near,
                });
            }
            Some(i) => i,
        };

        // Route to the index's verified client, posting at the index's rotation
        // SNI so completion + signature land on the same backend.
        let url = self
            .rotation_url(index as u64, "/v1/chat/completions")
            .unwrap_or_else(|| format!("{}/v1/chat/completions", self.config.base_url));
        let index_client = self.get_or_verify_index_client(index).await?;

        let send = |client: &Client, hdrs: reqwest::header::HeaderMap| {
            client
                .post(&url)
                .headers(hdrs)
                .json(&non_streaming_params)
                .timeout(timeout)
                .send()
        };

        let response = match send(&index_client, headers.clone()).await {
            Ok(r) => r,
            // Connection dropped or fingerprint mismatch on reconnect — clear
            // the index client and re-verify with a fresh attestation. Two
            // subtleties:
            // - Read/request timeouts must NOT enter this branch: in reqwest
            //   0.12 a per-request timeout stringifies as "error sending
            //   request for url (...): operation timed out", which matches the
            //   substring check; without `!is_timeout() || is_connect()` we'd
            //   burn another full timeout cycle on a doomed retry.
            // - Connect timeouts (`is_timeout && is_connect`) DO enter, since
            //   they're worth retrying — likely network blip, fresh backend.
            Err(e)
                if (!e.is_timeout() || e.is_connect())
                    && (e.is_connect()
                        || e.to_string()
                            .contains("does not match any attested fingerprint")
                        || e.to_string().contains("error sending request")) =>
            {
                self.clear_index(index);
                let fresh = self.get_or_verify_index_client(index).await?;
                send(&fresh, headers.clone()).await.map_err(map_send_err)?
            }
            Err(e) => return Err(map_send_err(e)),
        };

        if !response.status().is_success() {
            let status = response.status();
            let status_code = status.as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
            let canonical_err = CompletionError::HttpError {
                status_code,
                message: crate::extract_error_message(&error_text),
                is_external: false,
            };
            // The sticky index landed on a backend whose queue is full (or is
            // otherwise reporting 5xx/429). Walk the other backends ordered by
            // EMA via their pooled, verified index clients. If one is healthy,
            // the request succeeds and we record the index for signature
            // retrieval.
            if Fleet::is_rotation_retryable_status(status_code) {
                return self
                    .try_chat_completion_fallback_indices(
                        &self.fallback_indices(index),
                        &non_streaming_params,
                        &headers,
                        timeout,
                        canonical_err,
                    )
                    .await;
            }
            return Err(canonical_err);
        }

        // Get the raw bytes first for exact hash verification
        let raw_bytes = response.bytes().await.map_err(map_send_err)?.to_vec();

        // Parse the response from the raw bytes
        let chat_completion_response: ChatCompletionResponse = serde_json::from_slice(&raw_bytes)
            .map_err(|e| {
            CompletionError::CompletionError(format!("Failed to parse response: {e}"))
        })?;

        // Store the effective backend index for signature fetching.
        // For non-streaming, we know the chat_id immediately.
        let chat_id = chat_completion_response.id.clone();
        self.signature_rotation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(chat_id, index as u64);

        Ok(ChatCompletionResponseWithBytes {
            response: chat_completion_response,
            raw_bytes,
            serving_tier: crate::ProviderTier::Near,
        })
    }

    /// Performs a streaming text completion request
    async fn text_completion_stream(
        &self,
        params: CompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        let url = format!("{}/v1/completions", self.config.base_url);

        // Ensure streaming and token usage are enabled
        let mut streaming_params = params;
        streaming_params.stream = Some(true);
        streaming_params.stream_options = Some(StreamOptions {
            include_usage: Some(true),
            continuous_usage_stats: Some(true),
            extra: Default::default(),
        });

        let headers = self
            .build_headers()
            .map_err(CompletionError::CompletionError)?;
        let response = self
            .send_streaming_request(&url, headers, &streaming_params, None)
            .await?;

        // Use the SSE parser to handle the stream properly
        let sse_stream = new_sse_parser(response.bytes_stream(), false);
        Ok(Box::pin(sse_stream))
    }

    /// Performs an image generation request
    async fn image_generation(
        &self,
        mut params: ImageGenerationParams,
        request_hash: String,
    ) -> Result<ImageGenerationResponseWithBytes, ImageGenerationError> {
        let url = format!("{}/v1/images/generations", self.config.base_url);

        let mut headers = self.build_headers().map_err(to_image_gen_error)?;

        headers.insert(
            "X-Request-Hash",
            HeaderValue::from_str(&request_hash).map_err(to_image_gen_error)?,
        );

        // Forward tracing and encryption headers from extra to HTTP headers
        self.prepare_tracing_headers(&mut headers, &mut params.extra);
        self.prepare_encryption_headers(&mut headers, &mut params.extra);

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&params)
            .timeout(Duration::from_secs(180))
            .send()
            .await
            .map_err(to_image_gen_error)?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(ImageGenerationError::HttpError {
                status_code,
                message,
            });
        }

        // Get raw bytes first for exact hash verification (same pattern as chat_completion)
        let raw_bytes = response.bytes().await.map_err(to_image_gen_error)?.to_vec();

        // Parse the response from the raw bytes
        let image_response: ImageGenerationResponse =
            serde_json::from_slice(&raw_bytes).map_err(to_image_gen_error)?;

        Ok(ImageGenerationResponseWithBytes {
            response: image_response,
            raw_bytes,
        })
    }

    async fn audio_transcription(
        &self,
        mut params: AudioTranscriptionParams,
        request_hash: String,
    ) -> Result<AudioTranscriptionResponse, AudioTranscriptionError> {
        let url = format!("{}/v1/audio/transcriptions", self.config.base_url);

        // Detect content type from filename
        let content_type = crate::models::detect_audio_content_type(&params.filename);

        // Build multipart form
        let file_part = reqwest::multipart::Part::bytes(params.file_bytes)
            .file_name(params.filename.clone())
            .mime_str(&content_type)
            .map_err(|e| AudioTranscriptionError::TranscriptionError(e.to_string()))?;

        let mut form = reqwest::multipart::Form::new()
            .part("file", file_part)
            .text("model", params.model.clone());

        if let Some(language) = params.language {
            form = form.text("language", language);
        }

        if let Some(response_format) = params.response_format {
            form = form.text("response_format", response_format);
        }

        if let Some(temperature) = params.temperature {
            form = form.text("temperature", temperature.to_string());
        }

        if let Some(granularities) = params.timestamp_granularities {
            for granularity in granularities {
                form = form.text("timestamp_granularities[]", granularity);
            }
        }

        // Build headers (no Content-Type - reqwest sets it automatically for multipart)
        let mut headers = self
            .build_headers()
            .map_err(|e| AudioTranscriptionError::TranscriptionError(e.to_string()))?;
        // Forward tracing and encryption headers from extra to HTTP headers
        self.prepare_tracing_headers(&mut headers, &mut params.extra);
        self.prepare_encryption_headers(&mut headers, &mut params.extra);
        // Remove Content-Type header - reqwest will set it automatically for multipart
        headers.remove("Content-Type");
        headers.insert(
            "X-Request-Hash",
            HeaderValue::from_str(&request_hash)
                .map_err(|e| AudioTranscriptionError::TranscriptionError(e.to_string()))?,
        );

        // Send request with timeout
        let response = self
            .client
            .post(&url)
            .headers(headers)
            .multipart(form)
            .timeout(self.config.completion_timeout())
            .send()
            .await
            .map_err(|e| {
                tracing::debug!(
                    error_type = %e.status().map(|s| s.as_u16()).unwrap_or(0),
                    is_timeout = e.is_timeout(),
                    is_connect = e.is_connect(),
                    "Audio transcription send failed"
                );
                AudioTranscriptionError::TranscriptionError(e.to_string())
            })?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            // Log genuine client-input 4xx (malformed/unsupported/oversized
            // audio) at warn so a burst of bad uploads does not page on-call.
            // Everything else — 401/403 (our backend creds), 404 (missing/stale
            // route), 408 (timeout), 429, and 5xx — is a real infra/transient
            // fault and stays at error so it still alerts.
            if is_client_audio_input_status(status_code) {
                tracing::warn!(
                    status_code,
                    "Audio transcription request rejected by provider (client input)"
                );
            } else {
                tracing::error!(
                    status_code,
                    "Audio transcription request failed with HTTP error"
                );
            }
            return Err(AudioTranscriptionError::HttpError {
                status_code,
                message,
            });
        }

        let transcription_response: AudioTranscriptionResponse =
            response.json().await.map_err(|e| {
                tracing::debug!(
                    error_type = %e,
                    "Audio transcription response deserialization failed"
                );
                AudioTranscriptionError::TranscriptionError(e.to_string())
            })?;

        Ok(transcription_response)
    }

    /// Performs an image edit request
    async fn image_edit(
        &self,
        params: Arc<ImageEditParams>,
        request_hash: String,
    ) -> Result<ImageEditResponseWithBytes, ImageEditError> {
        let url = format!("{}/v1/images/edits", self.config.base_url);

        // Build headers without Content-Type (let reqwest set multipart boundary)
        let mut headers = reqwest::header::HeaderMap::new();

        if let Some(ref api_key) = self.config.api_key {
            let auth_value = format!("Bearer {api_key}");
            let header_value = HeaderValue::from_str(&auth_value)
                .map_err(|e| ImageEditError::EditError(format!("Invalid API key format: {e}")))?;
            headers.insert("Authorization", header_value);
        }

        headers.insert(
            "X-Request-Hash",
            HeaderValue::from_str(&request_hash)
                .map_err(|e| ImageEditError::EditError(format!("Invalid request hash: {e}")))?,
        );

        // Dereference Arc<Vec<u8>> to get &[u8] for efficient handling
        let image_data: &[u8] = &params.image;

        // Detect image MIME type based on magic bytes
        let image_mime_type = if image_data.len() >= 3 && &image_data[0..3] == b"\xFF\xD8\xFF" {
            "image/jpeg"
        } else if image_data.len() >= 4 && &image_data[0..4] == b"\x89PNG" {
            "image/png"
        } else {
            "image/jpeg" // Default to jpeg
        };

        // Build multipart form data
        let mut form = reqwest::multipart::Form::new();

        // Add text fields first (clone strings since Arc doesn't allow moving)
        form = form.text("model", params.model.clone());
        form = form.text("prompt", params.prompt.clone());

        // Add image as image[] field (vLLM expects array syntax)
        let image_part = reqwest::multipart::Part::bytes(image_data.to_vec())
            .file_name("image.bin")
            .mime_str(image_mime_type)
            .map_err(|e| ImageEditError::EditError(format!("Invalid image MIME type: {e}")))?;
        form = form.part("image[]", image_part);

        // Add optional text parameters
        if let Some(size) = params.size.as_ref() {
            form = form.text("size", size.clone());
        }
        if let Some(response_format) = params.response_format.as_ref() {
            form = form.text("response_format", response_format.clone());
        }

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .multipart(form)
            .timeout(Duration::from_secs(180))
            .send()
            .await
            .map_err(|e| ImageEditError::EditError(format!("Request failed: {e}")))?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(ImageEditError::HttpError {
                status_code,
                message,
            });
        }

        // Get raw bytes first for exact hash verification (same pattern as image_generation)
        let raw_bytes = response
            .bytes()
            .await
            .map_err(|e| ImageEditError::EditError(format!("Failed to read response body: {e}")))?
            .to_vec();

        // Parse the response from the raw bytes
        let edit_response: ImageGenerationResponse = serde_json::from_slice(&raw_bytes)
            .map_err(|e| ImageEditError::EditError(format!("Failed to parse response: {e}")))?;

        Ok(ImageEditResponseWithBytes {
            response: edit_response,
            raw_bytes,
        })
    }

    /// Performs a document reranking request
    async fn score(
        &self,
        mut params: ScoreParams,
        request_hash: String,
    ) -> Result<ScoreResponse, ScoreError> {
        let url = format!("{}/v1/score", self.config.base_url);

        let mut headers = self.build_headers().map_err(to_score_error)?;
        self.prepare_tracing_headers(&mut headers, &mut params.extra);
        self.prepare_encryption_headers(&mut headers, &mut params.extra);
        headers.insert(
            "X-Request-Hash",
            reqwest::header::HeaderValue::from_str(&request_hash).map_err(to_score_error)?,
        );

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&params)
            .timeout(self.config.completion_timeout())
            .send()
            .await
            .map_err(to_score_error)?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(ScoreError::HttpError {
                status_code,
                message,
            });
        }

        let score_response: ScoreResponse = response.json().await.map_err(to_score_error)?;
        Ok(score_response)
    }

    async fn rerank(&self, mut params: RerankParams) -> Result<RerankResponse, RerankError> {
        let url = format!("{}/v1/rerank", self.config.base_url);

        let mut headers = self.build_headers().map_err(to_rerank_error)?;
        self.prepare_tracing_headers(&mut headers, &mut params.extra);
        self.prepare_encryption_headers(&mut headers, &mut params.extra);

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&params)
            .timeout(self.config.completion_timeout())
            .send()
            .await
            .map_err(to_rerank_error)?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(RerankError::HttpError {
                status_code,
                message,
            });
        }

        let rerank_response: RerankResponse = response.json().await.map_err(to_rerank_error)?;
        Ok(rerank_response)
    }

    async fn embeddings_raw(
        &self,
        body: bytes::Bytes,
        mut extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, EmbeddingError> {
        let url = format!("{}/v1/embeddings", self.config.base_url);

        let mut headers = self.build_headers().map_err(to_embedding_error)?;
        self.prepare_tracing_headers(&mut headers, &mut extra);
        self.prepare_encryption_headers(&mut headers, &mut extra);

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .timeout(self.config.completion_timeout())
            .send()
            .await
            .map_err(to_embedding_error)?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(EmbeddingError::HttpError {
                status_code,
                message: crate::extract_error_message(&error_text),
            });
        }

        let raw_bytes = response.bytes().await.map_err(to_embedding_error)?;
        Ok(raw_bytes)
    }

    async fn privacy_classify_raw(
        &self,
        body: bytes::Bytes,
        mut extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, PrivacyClassifyError> {
        let url = format!("{}/v1/privacy/classify", self.config.base_url);

        let mut headers = self.build_headers().map_err(to_privacy_classify_error)?;
        self.prepare_tracing_headers(&mut headers, &mut extra);
        self.prepare_encryption_headers(&mut headers, &mut extra);

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .timeout(self.config.completion_timeout())
            .send()
            .await
            .map_err(to_privacy_classify_error)?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(PrivacyClassifyError::HttpError {
                status_code,
                message,
            });
        }

        let raw_bytes = response.bytes().await.map_err(to_privacy_classify_error)?;
        Ok(raw_bytes)
    }
}

/// Provider is a thin trait adapter: every InferenceProvider call delegates
/// to its Fleet, which holds all NEAR-AI model-proxy state and logic.
#[async_trait]
impl InferenceProvider for Provider {
    /// NEAR AI's own attested TEE fleet — the primary tier for any model NEAR
    /// serves; an attested third party (Chutes) sits behind it as fallback.
    fn tier(&self) -> crate::ProviderTier {
        crate::ProviderTier::Near
    }

    fn provider_source(&self) -> crate::ProviderSource {
        crate::ProviderSource::Vllm
    }

    async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        self.fleet.models().await
    }
    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        self.fleet
            .chat_completion_stream(params, request_hash)
            .await
    }
    async fn chat_completion(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        self.fleet.chat_completion(params, request_hash).await
    }
    async fn text_completion_stream(
        &self,
        params: CompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        self.fleet.text_completion_stream(params).await
    }
    async fn image_generation(
        &self,
        params: ImageGenerationParams,
        request_hash: String,
    ) -> Result<ImageGenerationResponseWithBytes, ImageGenerationError> {
        self.fleet.image_generation(params, request_hash).await
    }
    async fn image_edit(
        &self,
        params: Arc<ImageEditParams>,
        request_hash: String,
    ) -> Result<ImageEditResponseWithBytes, ImageEditError> {
        self.fleet.image_edit(params, request_hash).await
    }
    async fn score(
        &self,
        params: ScoreParams,
        request_hash: String,
    ) -> Result<ScoreResponse, ScoreError> {
        self.fleet.score(params, request_hash).await
    }
    async fn rerank(&self, params: RerankParams) -> Result<RerankResponse, RerankError> {
        self.fleet.rerank(params).await
    }
    async fn embeddings_raw(
        &self,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, EmbeddingError> {
        self.fleet.embeddings_raw(body, extra).await
    }
    async fn privacy_classify_raw(
        &self,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, PrivacyClassifyError> {
        self.fleet.privacy_classify_raw(body, extra).await
    }
    async fn get_signature(
        &self,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<ChatSignature, CompletionError> {
        self.fleet.get_signature(chat_id, signing_algo).await
    }
    fn pin_chat_connection(&self, request_hash: &str, chat_id: &str) {
        self.fleet.pin_chat_connection(request_hash, chat_id)
    }
    fn unpin_chat_connection(&self, chat_id: &str) {
        self.fleet.unpin_chat_connection(chat_id)
    }
    fn set_backend_count(&self, count: usize) {
        self.fleet.set_backend_count(count)
    }
    async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
    ) -> Result<serde_json::Map<String, serde_json::Value>, AttestationError> {
        self.fleet
            .get_attestation_report(
                model,
                signing_algo,
                nonce,
                signing_address,
                include_tls_fingerprint,
            )
            .await
    }
    async fn audio_transcription(
        &self,
        params: AudioTranscriptionParams,
        request_hash: String,
    ) -> Result<AudioTranscriptionResponse, AudioTranscriptionError> {
        self.fleet.audio_transcription(params, request_hash).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn control_event(raw: &'static str) -> SSEEvent {
        SSEEvent {
            raw_bytes: bytes::Bytes::from_static(raw.as_bytes()),
            chunk: None,
            raw_passthrough: true,
        }
    }

    fn data_event() -> SSEEvent {
        SSEEvent {
            raw_bytes: bytes::Bytes::from_static(b"data: {}\n"),
            chunk: Some(StreamChunk::Chat(ChatCompletionChunk {
                id: "chat-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 0,
                model: "test".to_string(),
                choices: vec![],
                usage: None,
                prompt_token_ids: None,
                system_fingerprint: None,
                modality: None,
                extra: Default::default(),
            })),
            raw_passthrough: true,
        }
    }

    /// A leading control event (keepalive comment) must not mask a
    /// first-payload in-stream error frame: rotation classification has to
    /// skip past chunk-less events, and the skipped events must be
    /// re-attached so the byte stream stays exact (issue #701).
    #[tokio::test]
    async fn peek_first_payload_status_skips_leading_control_events() {
        let items: Vec<Result<SSEEvent, CompletionError>> = vec![
            Ok(control_event(": keepalive\n")),
            Ok(control_event("\n")),
            Err(CompletionError::HttpError {
                status_code: 503,
                message: "queue full".to_string(),
                is_external: false,
            }),
        ];
        let stream: StreamingResult = Box::pin(futures_util::stream::iter(items));
        let (status, stream) = Fleet::peek_first_payload_status(stream).await;
        assert_eq!(
            status,
            Some(503),
            "Control events must not mask a retryable first-payload error"
        );

        // The consumed control events must still come out of the returned
        // stream, in order, before the error.
        let replayed: Vec<Result<SSEEvent, CompletionError>> =
            futures_util::StreamExt::collect(stream).await;
        assert_eq!(replayed.len(), 3);
        assert_eq!(
            replayed[0].as_ref().unwrap().raw_bytes.as_ref(),
            b": keepalive\n"
        );
        assert_eq!(replayed[1].as_ref().unwrap().raw_bytes.as_ref(), b"\n");
        assert!(matches!(
            replayed[2],
            Err(CompletionError::HttpError {
                status_code: 503,
                ..
            })
        ));
    }

    fn audio_transcription_params() -> AudioTranscriptionParams {
        AudioTranscriptionParams {
            model: "openai/whisper-large-v3".to_string(),
            file_bytes: vec![1, 2, 3],
            filename: "audio.mp3".to_string(),
            language: Some("en".to_string()),
            response_format: Some("verbose_json".to_string()),
            temperature: None,
            timestamp_granularities: Some(vec!["word".to_string(), "segment".to_string()]),
            extra: Default::default(),
        }
    }

    #[tokio::test]
    async fn audio_transcription_sends_repeated_timestamp_granularity_fields() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "text": "ok",
                "duration": 1.0,
                "words": [{"word": "ok", "start": 0.0, "end": 1.0}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = Provider::new(Config::new(
            server.uri(),
            Some("sk-test".to_string()),
            Some(5),
        ));

        let response = provider
            .audio_transcription(audio_transcription_params(), "request-hash".to_string())
            .await
            .unwrap();

        assert_eq!(response.text, "ok");
        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&requests[0].body);
        assert_eq!(
            body.matches("name=\"timestamp_granularities[]\"").count(),
            2
        );
        assert!(body.contains("\r\nword\r\n"), "body was: {body}");
        assert!(body.contains("\r\nsegment\r\n"), "body was: {body}");
        assert!(!body.contains("word,segment"), "body was: {body}");
    }

    /// Happy path: first payload is a parsed data chunk — no rotation, and
    /// the stream is returned intact.
    #[tokio::test]
    async fn peek_first_payload_status_data_first_returns_none() {
        let items: Vec<Result<SSEEvent, CompletionError>> =
            vec![Ok(control_event(": ping\n")), Ok(data_event())];
        let stream: StreamingResult = Box::pin(futures_util::stream::iter(items));
        let (status, stream) = Fleet::peek_first_payload_status(stream).await;
        assert_eq!(status, None);
        let replayed: Vec<Result<SSEEvent, CompletionError>> =
            futures_util::StreamExt::collect(stream).await;
        assert_eq!(replayed.len(), 2);
        assert!(replayed[0].as_ref().unwrap().chunk.is_none());
        assert!(replayed[1].as_ref().unwrap().chunk.is_some());
    }

    /// A non-retryable first-payload error (e.g. 400) must not trigger
    /// rotation.
    #[tokio::test]
    async fn peek_first_payload_status_non_retryable_error_returns_none() {
        let items: Vec<Result<SSEEvent, CompletionError>> = vec![Err(CompletionError::HttpError {
            status_code: 400,
            message: "bad request".to_string(),
            is_external: false,
        })];
        let stream: StreamingResult = Box::pin(futures_util::stream::iter(items));
        let (status, _stream) = Fleet::peek_first_payload_status(stream).await;
        assert_eq!(status, None);
    }

    #[derive(Debug)]
    struct ChainedErr {
        msg: &'static str,
        source: Option<Box<dyn std::error::Error + 'static>>,
    }

    impl std::fmt::Display for ChainedErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.msg)
        }
    }

    impl std::error::Error for ChainedErr {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source.as_deref()
        }
    }

    #[test]
    fn format_error_chain_flat_error() {
        let e = ChainedErr {
            msg: "outer",
            source: None,
        };
        assert_eq!(format_error_chain(&e), "outer");
    }

    #[test]
    fn format_error_chain_walks_all_sources() {
        let inner = ChainedErr {
            msg: "broken pipe",
            source: None,
        };
        let middle = ChainedErr {
            msg: "connection closed before message completed",
            source: Some(Box::new(inner)),
        };
        let outer = ChainedErr {
            msg: "error sending request for url (https://x/v1/signature/y)",
            source: Some(Box::new(middle)),
        };
        assert_eq!(
            format_error_chain(&outer),
            "error sending request for url (https://x/v1/signature/y)\
             : caused by: connection closed before message completed\
             : caused by: broken pipe"
        );
    }

    fn create_test_provider() -> Provider {
        Provider::new(Config {
            base_url: "http://localhost".to_string(),
            api_key: None,
            completion_timeout_seconds: 30,
            control_timeout_seconds: 30,
        })
    }

    /// Helper that scrubs both timeout env vars before/after a closure runs,
    /// preventing parent shell exports from leaking into the test.
    ///
    /// TODO(rust 1.81+): `std::env::set_var` / `remove_var` become `unsafe` to
    /// call (parallel-process env-mutation is not race-free). Either wrap with
    /// `unsafe { ... }` and rely on `#[serial]` to serialize, or migrate to
    /// the `temp-env` crate which encapsulates the unsafety.
    fn with_clean_timeout_env<R>(f: impl FnOnce() -> R) -> R {
        let prev_completion = std::env::var("VLLM_PROVIDER_COMPLETION_TIMEOUT").ok();
        let prev_control = std::env::var("VLLM_PROVIDER_CONTROL_TIMEOUT").ok();
        std::env::remove_var("VLLM_PROVIDER_COMPLETION_TIMEOUT");
        std::env::remove_var("VLLM_PROVIDER_CONTROL_TIMEOUT");
        let result = f();
        match prev_completion {
            Some(v) => std::env::set_var("VLLM_PROVIDER_COMPLETION_TIMEOUT", v),
            None => std::env::remove_var("VLLM_PROVIDER_COMPLETION_TIMEOUT"),
        }
        match prev_control {
            Some(v) => std::env::set_var("VLLM_PROVIDER_CONTROL_TIMEOUT", v),
            None => std::env::remove_var("VLLM_PROVIDER_CONTROL_TIMEOUT"),
        }
        result
    }

    #[test]
    #[serial]
    fn vllm_config_uses_default_timeouts_when_env_unset() {
        with_clean_timeout_env(|| {
            let cfg = Config::new("http://x".to_string(), None, None);
            assert_eq!(
                cfg.completion_timeout_seconds,
                Config::DEFAULT_COMPLETION_TIMEOUT_SECS
            );
            assert_eq!(
                cfg.control_timeout_seconds,
                Config::DEFAULT_CONTROL_TIMEOUT_SECS
            );
            assert_eq!(
                cfg.completion_timeout(),
                Duration::from_secs(Config::DEFAULT_COMPLETION_TIMEOUT_SECS as u64)
            );
            assert_eq!(
                cfg.control_timeout(),
                Duration::from_secs(Config::DEFAULT_CONTROL_TIMEOUT_SECS as u64)
            );
        });
    }

    #[test]
    #[serial]
    fn vllm_config_reads_env_vars_when_present() {
        with_clean_timeout_env(|| {
            std::env::set_var("VLLM_PROVIDER_COMPLETION_TIMEOUT", "1234");
            std::env::set_var("VLLM_PROVIDER_CONTROL_TIMEOUT", "42");
            let cfg = Config::new("http://x".to_string(), None, None);
            assert_eq!(cfg.completion_timeout_seconds, 1234);
            assert_eq!(cfg.control_timeout_seconds, 42);
        });
    }

    #[test]
    #[serial]
    fn vllm_config_positional_arg_overrides_completion_env() {
        with_clean_timeout_env(|| {
            std::env::set_var("VLLM_PROVIDER_COMPLETION_TIMEOUT", "1234");
            std::env::set_var("VLLM_PROVIDER_CONTROL_TIMEOUT", "42");
            // Positional `Some(N)` keeps the legacy meaning: it sets completion only,
            // overriding the env. Control still reads from env.
            let cfg = Config::new("http://x".to_string(), None, Some(7));
            assert_eq!(cfg.completion_timeout_seconds, 7);
            assert_eq!(cfg.control_timeout_seconds, 42);
        });
    }

    #[test]
    #[serial]
    fn vllm_config_falls_back_to_default_on_unparseable_env() {
        with_clean_timeout_env(|| {
            std::env::set_var("VLLM_PROVIDER_COMPLETION_TIMEOUT", "not-a-number");
            std::env::set_var("VLLM_PROVIDER_CONTROL_TIMEOUT", "");
            let cfg = Config::new("http://x".to_string(), None, None);
            assert_eq!(
                cfg.completion_timeout_seconds,
                Config::DEFAULT_COMPLETION_TIMEOUT_SECS
            );
            assert_eq!(
                cfg.control_timeout_seconds,
                Config::DEFAULT_CONTROL_TIMEOUT_SECS
            );
        });
    }

    #[test]
    fn vllm_config_negative_timeout_clamped_to_zero_duration() {
        let cfg = Config {
            base_url: "http://x".to_string(),
            api_key: None,
            completion_timeout_seconds: -5,
            control_timeout_seconds: -10,
        };
        // Conversion to Duration must not panic on negative values.
        assert_eq!(cfg.completion_timeout(), Duration::ZERO);
        assert_eq!(cfg.control_timeout(), Duration::ZERO);
    }

    #[test]
    fn timeout_error_display_includes_operation_and_seconds() {
        let err = CompletionError::Timeout {
            operation: "chat_completion".to_string(),
            timeout_seconds: 600,
        };
        let s = err.to_string();
        assert!(s.contains("chat_completion"), "got: {s}");
        assert!(s.contains("600"), "got: {s}");
    }

    #[test]
    fn test_prepare_tracing_headers_removes_keys_from_extra() {
        let provider = create_test_provider();
        let mut headers = reqwest::header::HeaderMap::new();
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            tracing_headers::REQUEST_ID.to_string(),
            serde_json::Value::String("550e8400-e29b-41d4-a716-446655440000".to_string()),
        );
        extra.insert(
            tracing_headers::ORG_ID.to_string(),
            serde_json::Value::String("org-uuid".to_string()),
        );
        extra.insert(
            tracing_headers::WORKSPACE_ID.to_string(),
            serde_json::Value::String("ws-uuid".to_string()),
        );
        extra.insert(
            "other_field".to_string(),
            serde_json::Value::String("keep-me".to_string()),
        );

        provider
            .fleet
            .prepare_tracing_headers(&mut headers, &mut extra);

        assert!(
            !extra.contains_key(tracing_headers::REQUEST_ID),
            "x_request_id should be removed"
        );
        assert!(
            !extra.contains_key(tracing_headers::ORG_ID),
            "x_org_id should be removed"
        );
        assert!(
            !extra.contains_key(tracing_headers::WORKSPACE_ID),
            "x_workspace_id should be removed"
        );
        assert!(
            extra.contains_key("other_field"),
            "unrelated fields must be preserved"
        );
    }

    #[test]
    fn test_prepare_tracing_headers_forwards_to_http_headers() {
        let provider = create_test_provider();
        let mut headers = reqwest::header::HeaderMap::new();
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            tracing_headers::REQUEST_ID.to_string(),
            serde_json::Value::String("550e8400-e29b-41d4-a716-446655440000".to_string()),
        );
        extra.insert(
            tracing_headers::ORG_ID.to_string(),
            serde_json::Value::String("aaaa-bbbb".to_string()),
        );
        extra.insert(
            tracing_headers::WORKSPACE_ID.to_string(),
            serde_json::Value::String("cccc-dddd".to_string()),
        );

        provider
            .fleet
            .prepare_tracing_headers(&mut headers, &mut extra);

        assert_eq!(
            headers.get("X-Request-Id").and_then(|v| v.to_str().ok()),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            headers.get("X-Org-Id").and_then(|v| v.to_str().ok()),
            Some("aaaa-bbbb")
        );
        assert_eq!(
            headers.get("X-Workspace-Id").and_then(|v| v.to_str().ok()),
            Some("cccc-dddd")
        );
    }

    #[test]
    fn test_prepare_tracing_headers_absent_keys_are_noop() {
        let provider = create_test_provider();
        let mut headers = reqwest::header::HeaderMap::new();
        let mut extra: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();

        provider
            .fleet
            .prepare_tracing_headers(&mut headers, &mut extra);

        assert!(headers.get("X-Request-Id").is_none());
        assert!(headers.get("X-Org-Id").is_none());
        assert!(headers.get("X-Workspace-Id").is_none());
    }

    #[test]
    fn test_prepare_encryption_headers_removes_keys_from_extra() {
        let provider = create_test_provider();

        let mut headers = reqwest::header::HeaderMap::new();
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            encryption_headers::SIGNING_ALGO.to_string(),
            serde_json::Value::String("ecdsa".to_string()),
        );
        extra.insert(
            encryption_headers::CLIENT_PUB_KEY.to_string(),
            serde_json::Value::String("abc123".to_string()),
        );
        extra.insert(
            encryption_headers::MODEL_PUB_KEY.to_string(),
            serde_json::Value::String("def456".to_string()),
        );
        extra.insert(
            encryption_headers::ENCRYPTION_VERSION.to_string(),
            serde_json::Value::String("2".to_string()),
        );

        provider
            .fleet
            .prepare_encryption_headers(&mut headers, &mut extra);

        // Verify all encryption keys removed from extra
        assert!(
            !extra.contains_key(encryption_headers::SIGNING_ALGO),
            "x_signing_algo should be removed from extra"
        );
        assert!(
            !extra.contains_key(encryption_headers::CLIENT_PUB_KEY),
            "x_client_pub_key should be removed from extra"
        );
        assert!(
            !extra.contains_key(encryption_headers::MODEL_PUB_KEY),
            "x_model_pub_key should be removed from extra"
        );
        assert!(
            !extra.contains_key(encryption_headers::ENCRYPTION_VERSION),
            "x_encryption_version should be removed from extra"
        );
    }

    #[test]
    fn test_prepare_encryption_headers_forwards_to_http_headers() {
        let provider = create_test_provider();

        let mut headers = reqwest::header::HeaderMap::new();
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            encryption_headers::SIGNING_ALGO.to_string(),
            serde_json::Value::String("ecdsa".to_string()),
        );
        extra.insert(
            encryption_headers::CLIENT_PUB_KEY.to_string(),
            serde_json::Value::String("abc123".to_string()),
        );
        extra.insert(
            encryption_headers::MODEL_PUB_KEY.to_string(),
            serde_json::Value::String("def456".to_string()),
        );
        extra.insert(
            encryption_headers::ENCRYPTION_VERSION.to_string(),
            serde_json::Value::String("2".to_string()),
        );

        provider
            .fleet
            .prepare_encryption_headers(&mut headers, &mut extra);

        // Verify encryption headers forwarded (except model_pub_key)
        assert_eq!(
            headers.get("X-Signing-Algo").unwrap(),
            "ecdsa",
            "X-Signing-Algo header should be forwarded"
        );
        assert_eq!(
            headers.get("X-Client-Pub-Key").unwrap(),
            "abc123",
            "X-Client-Pub-Key header should be forwarded"
        );
        assert_eq!(
            headers.get("X-Encryption-Version").unwrap(),
            "2",
            "X-Encryption-Version header should be forwarded"
        );
        // model_pub_key should NOT be forwarded (used only for routing, not sent to vllm-proxy)
        assert!(
            headers.get("X-Model-Pub-Key").is_none(),
            "X-Model-Pub-Key should NOT be forwarded to HTTP headers"
        );
    }

    #[test]
    fn test_prepare_encryption_headers_preserves_other_extra_fields() {
        let provider = create_test_provider();

        let mut headers = reqwest::header::HeaderMap::new();
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            encryption_headers::SIGNING_ALGO.to_string(),
            serde_json::Value::String("ecdsa".to_string()),
        );
        extra.insert(
            "some_other_field".to_string(),
            serde_json::Value::String("should_remain".to_string()),
        );
        extra.insert(
            "another_field".to_string(),
            serde_json::Value::Number(serde_json::Number::from(42)),
        );

        provider
            .fleet
            .prepare_encryption_headers(&mut headers, &mut extra);

        // Encryption key should be removed
        assert!(!extra.contains_key(encryption_headers::SIGNING_ALGO));
        // Other fields should remain
        assert_eq!(
            extra.get("some_other_field"),
            Some(&serde_json::Value::String("should_remain".to_string())),
            "Non-encryption fields should be preserved in extra"
        );
        assert_eq!(
            extra.get("another_field"),
            Some(&serde_json::Value::Number(serde_json::Number::from(42))),
            "Non-encryption fields should be preserved in extra"
        );
    }

    /// This test documents the danger of serde(flatten) on extra fields.
    /// If encryption headers are NOT removed from extra before serialization,
    /// they WILL appear in the JSON body sent to vLLM.
    #[test]
    fn test_image_generation_params_flatten_behavior_leaks_extra_to_json() {
        let mut extra = std::collections::HashMap::new();
        // Simulate encryption headers that SHOULD have been removed
        extra.insert(
            encryption_headers::SIGNING_ALGO.to_string(),
            serde_json::Value::String("ecdsa".to_string()),
        );

        let params = ImageGenerationParams {
            model: "test-model".to_string(),
            prompt: "test prompt".to_string(),
            n: None,
            size: None,
            response_format: None,
            quality: None,
            style: None,
            extra,
        };

        let json = serde_json::to_string(&params).unwrap();

        // This test documents the DANGER: if encryption headers are NOT removed
        // from extra before serialization, they WILL appear in JSON due to flatten
        assert!(
            json.contains("x_signing_algo"),
            "Test demonstrates flatten behavior - encryption headers in extra leak to JSON body. \
             This is why prepare_encryption_headers MUST be called before serialization."
        );
    }

    /// Regression test: verifies that after prepare_encryption_headers is called,
    /// the serialized ImageGenerationParams will NOT contain encryption keys.
    #[test]
    fn test_image_generation_params_no_encryption_keys_after_preparation() {
        let provider = create_test_provider();

        let mut extra = std::collections::HashMap::new();
        extra.insert(
            encryption_headers::SIGNING_ALGO.to_string(),
            serde_json::Value::String("ecdsa".to_string()),
        );
        extra.insert(
            encryption_headers::CLIENT_PUB_KEY.to_string(),
            serde_json::Value::String("abc123".to_string()),
        );
        extra.insert(
            encryption_headers::MODEL_PUB_KEY.to_string(),
            serde_json::Value::String("def456".to_string()),
        );
        extra.insert(
            encryption_headers::ENCRYPTION_VERSION.to_string(),
            serde_json::Value::String("2".to_string()),
        );
        extra.insert(
            "some_valid_param".to_string(),
            serde_json::Value::String("value".to_string()),
        );

        let mut headers = reqwest::header::HeaderMap::new();
        provider
            .fleet
            .prepare_encryption_headers(&mut headers, &mut extra);

        let params = ImageGenerationParams {
            model: "test-model".to_string(),
            prompt: "test prompt".to_string(),
            n: None,
            size: None,
            response_format: None,
            quality: None,
            style: None,
            extra,
        };

        let json = serde_json::to_string(&params).unwrap();

        // After preparation, encryption keys should NOT appear in JSON
        assert!(
            !json.contains("x_signing_algo"),
            "x_signing_algo should NOT appear in serialized JSON after prepare_encryption_headers"
        );
        assert!(
            !json.contains("x_client_pub_key"),
            "x_client_pub_key should NOT appear in serialized JSON after prepare_encryption_headers"
        );
        assert!(
            !json.contains("x_model_pub_key"),
            "x_model_pub_key should NOT appear in serialized JSON after prepare_encryption_headers"
        );
        assert!(
            !json.contains("x_encryption_version"),
            "x_encryption_version should NOT appear in serialized JSON after prepare_encryption_headers"
        );

        // Valid params should still be present
        assert!(
            json.contains("some_valid_param"),
            "Non-encryption extra fields should still be serialized"
        );
    }

    #[test]
    fn test_index_client_count_is_max_fanout() {
        // Per-index clients are sized to the hard fan-out cap (one slot per
        // possible rotation index), independent of the prefix bucket count.
        let provider = create_test_provider();
        assert_eq!(
            provider.fleet.index_clients.len(),
            crate::rotation::MAX_FANOUT
        );
    }

    #[test]
    fn test_legacy_provider_eagerly_creates_index_clients() {
        // Without a verifier, index clients are eagerly pre-created (legacy path)
        let provider = create_test_provider();
        let guard = provider.fleet.index_clients[0]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            guard.is_some(),
            "Legacy provider should pre-create index clients"
        );
    }

    #[test]
    fn test_lazy_index_clients_start_empty_with_verifier() {
        use std::sync::Arc;
        struct NoopVerifier;
        #[async_trait::async_trait]
        impl crate::BackendVerifier for NoopVerifier {
            async fn create_verified_client(
                &self,
                _base_url: &str,
            ) -> Result<reqwest::Client, String> {
                Ok(reqwest::Client::new())
            }
        }

        let provider = Provider::new_with_verifier(
            Config {
                base_url: "http://localhost".to_string(),
                api_key: None,
                completion_timeout_seconds: 30,
                control_timeout_seconds: 30,
            },
            Arc::new(std::sync::RwLock::new(
                crate::spki_verifier::FingerprintState::Bootstrap,
            )),
            Arc::new(NoopVerifier),
        );
        let guard = provider.fleet.index_clients[0]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            guard.is_none(),
            "Verifier-backed provider should start with empty index clients"
        );
    }

    #[tokio::test]
    async fn test_get_or_verify_fills_index_client() {
        use std::sync::Arc;
        struct NoopVerifier;
        #[async_trait::async_trait]
        impl crate::BackendVerifier for NoopVerifier {
            async fn create_verified_client(
                &self,
                _base_url: &str,
            ) -> Result<reqwest::Client, String> {
                Ok(reqwest::Client::new())
            }
        }

        let provider = Provider::new_with_verifier(
            Config {
                base_url: "http://localhost".to_string(),
                api_key: None,
                completion_timeout_seconds: 30,
                control_timeout_seconds: 30,
            },
            Arc::new(std::sync::RwLock::new(
                crate::spki_verifier::FingerprintState::Bootstrap,
            )),
            Arc::new(NoopVerifier),
        );

        // Bucket starts empty
        assert!(provider.fleet.index_clients[0].lock().unwrap().is_none());

        // get_or_verify fills it
        let result = provider.fleet.get_or_verify_index_client(0).await;
        assert!(result.is_ok());
        assert!(provider.fleet.index_clients[0].lock().unwrap().is_some());

        // Second call returns cached client (fast path)
        let result2 = provider.fleet.get_or_verify_index_client(0).await;
        assert!(result2.is_ok());
    }

    #[test]
    fn test_clear_index() {
        let provider = create_test_provider();
        assert!(provider.fleet.index_clients[0].lock().unwrap().is_some());
        provider.fleet.clear_index(0);
        assert!(provider.fleet.index_clients[0].lock().unwrap().is_none());
    }

    /// Fix 2 + security guard: when a verifier always fails AND no fingerprints
    /// have been pinned yet (Bootstrap state), get_or_verify_index_client must
    /// return Err — using the fallback_client in Bootstrap state would accept any
    /// WebPKI cert and silently bypass SPKI attestation in a TEE environment.
    #[tokio::test]
    async fn test_fallback_err_in_bootstrap_state() {
        use std::sync::Arc;
        struct AlwaysFailVerifier;
        #[async_trait::async_trait]
        impl crate::BackendVerifier for AlwaysFailVerifier {
            async fn create_verified_client(
                &self,
                _base_url: &str,
            ) -> Result<reqwest::Client, String> {
                Err("simulated attestation timeout".to_string())
            }
        }

        let provider = Provider::new_with_verifier(
            Config {
                base_url: "http://localhost".to_string(),
                api_key: None,
                completion_timeout_seconds: 30,
                control_timeout_seconds: 30,
            },
            Arc::new(std::sync::RwLock::new(
                crate::spki_verifier::FingerprintState::Bootstrap,
            )),
            Arc::new(AlwaysFailVerifier),
        );

        // Bucket starts empty and no fingerprints are pinned.
        assert!(provider.fleet.index_clients[0].lock().unwrap().is_none());
        assert_eq!(provider.pinned_fingerprint_count(), 0);

        // All attempts fail in Bootstrap state → must return Err (not fallback).
        let result = provider.fleet.get_or_verify_index_client(0).await;
        assert!(
            result.is_err(),
            "expected Err in Bootstrap state, got: {result:?}"
        );

        // Bucket remains empty.
        assert!(provider.fleet.index_clients[0].lock().unwrap().is_none());
    }

    /// Fix 2: when a verifier always fails but at least one fingerprint has already
    /// been pinned (Pinned state), the fallback_client is returned so the request
    /// degrades gracefully instead of returning "All providers failed". The fallback
    /// client's TLS verifier enforces SPKI pinning for any new connections.
    #[tokio::test]
    async fn test_fallback_ok_after_fingerprints_pinned() {
        use std::sync::Arc;
        struct AlwaysFailVerifier;
        #[async_trait::async_trait]
        impl crate::BackendVerifier for AlwaysFailVerifier {
            async fn create_verified_client(
                &self,
                _base_url: &str,
            ) -> Result<reqwest::Client, String> {
                Err("simulated attestation timeout".to_string())
            }
        }

        let provider = Provider::new_with_verifier(
            Config {
                base_url: "http://localhost".to_string(),
                api_key: None,
                completion_timeout_seconds: 30,
                control_timeout_seconds: 30,
            },
            Arc::new(std::sync::RwLock::new(
                crate::spki_verifier::FingerprintState::Bootstrap,
            )),
            Arc::new(AlwaysFailVerifier),
        );

        // Simulate a prior discovery cycle that pinned a fingerprint.
        provider.add_verified_fingerprint("deadbeef".to_string());
        assert_eq!(provider.pinned_fingerprint_count(), 1);

        // Bucket starts empty.
        assert!(provider.fleet.index_clients[0].lock().unwrap().is_none());

        // All attempts fail but fingerprints are pinned → fallback client returned.
        let result = provider.fleet.get_or_verify_index_client(0).await;
        assert!(result.is_ok(), "expected fallback Ok, got: {result:?}");

        // Bucket remains empty — fallback is not stored as a verified bucket client.
        assert!(
            provider.fleet.index_clients[0].lock().unwrap().is_none(),
            "fallback should not be stored in bucket"
        );
    }

    /// Fix 2 + security guard: in Blocked state (explicit attestation failure),
    /// `pinned_fingerprint_count()` returns 0, so the code takes the same safe
    /// path as Bootstrap and returns Err rather than the fallback client.
    #[tokio::test]
    async fn test_fallback_err_in_blocked_state() {
        use std::sync::Arc;
        struct AlwaysFailVerifier;
        #[async_trait::async_trait]
        impl crate::BackendVerifier for AlwaysFailVerifier {
            async fn create_verified_client(
                &self,
                _base_url: &str,
            ) -> Result<reqwest::Client, String> {
                Err("simulated attestation failure".to_string())
            }
        }

        let provider = Provider::new_with_verifier(
            Config {
                base_url: "http://localhost".to_string(),
                api_key: None,
                completion_timeout_seconds: 30,
                control_timeout_seconds: 30,
            },
            Arc::new(std::sync::RwLock::new(
                crate::spki_verifier::FingerprintState::Bootstrap,
            )),
            Arc::new(AlwaysFailVerifier),
        );

        // Transition to Blocked state (attestation explicitly failed).
        provider.block_connections();
        assert_eq!(provider.pinned_fingerprint_count(), 0);

        // Bucket starts empty.
        assert!(provider.fleet.index_clients[0].lock().unwrap().is_none());

        // Blocked state has pinned_count == 0 → same safe path as Bootstrap → Err.
        let result = provider.fleet.get_or_verify_index_client(0).await;
        assert!(
            result.is_err(),
            "expected Err in Blocked state, got: {result:?}"
        );
    }

    /// Fix 1: the semaphore serialises concurrent verifications so that only
    /// N attempts run at once. When the first succeeds and fills the bucket,
    /// later waiters take the fast path (bucket already filled) rather than
    /// running their own verification.
    ///
    /// Uses `new_with_verifier_and_concurrency` to set concurrency=1 without
    /// mutating env vars (which would be a data race in a parallel test suite).
    #[tokio::test]
    async fn test_semaphore_prevents_redundant_verification() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        struct CountingVerifier {
            count: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl crate::BackendVerifier for CountingVerifier {
            async fn create_verified_client(
                &self,
                _base_url: &str,
            ) -> Result<reqwest::Client, String> {
                self.count.fetch_add(1, Ordering::SeqCst);
                Ok(reqwest::Client::new())
            }
        }

        // concurrency=1 means verifications are fully serialised. Pass the value
        // directly rather than via env var to avoid races with parallel tests.
        let provider = Arc::new(Provider::new_with_verifier_and_concurrency(
            Config {
                base_url: "http://localhost".to_string(),
                api_key: None,
                completion_timeout_seconds: 30,
                control_timeout_seconds: 30,
            },
            Arc::new(std::sync::RwLock::new(
                crate::spki_verifier::FingerprintState::Bootstrap,
            )),
            Arc::new(CountingVerifier {
                count: call_count_clone,
            }),
            1, // inline_verify_concurrency
        ));

        // Spawn 8 concurrent requests all targeting bucket 0.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = provider.clone();
            handles.push(tokio::spawn(async move {
                p.fleet.get_or_verify_index_client(0).await
            }));
        }
        for h in handles {
            assert!(h.await.unwrap().is_ok());
        }

        // With a serialised semaphore, only the first waiter verifies; all
        // subsequent ones find the bucket already filled and skip verification.
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "only one verification call expected; redundant calls indicate the \
             semaphore double-check is not working"
        );
    }

    /// Regression test: a non-streaming `chat_completion` that hits the
    /// per-request timeout must NOT fall into the bucket-clear retry branch,
    /// because reqwest 0.12 stringifies a timeout as "error sending request
    /// for url (...): operation timed out" — a substring of the connect-retry
    /// guard. Without the `!is_timeout()` guard, a timeout doubles end-to-end
    /// latency before the pool's no-retry classifier sees `Timeout`.
    ///
    /// Asserts on the *connection count* (exactly one TCP accept = no retry; a
    /// retry would open a second), not on wall-clock elapsed — the behavioral
    /// check is deterministic, so the test is immune to test-harness CPU load.
    /// An earlier wall-clock bound flaked under the parallel pool, and
    /// `#[serial]` does not help: it only serializes against other `#[serial]`
    /// tests, not the non-serial async load that actually skews the timing.
    #[tokio::test]
    async fn test_timeout_does_not_trigger_bucket_clear_retry() {
        use crate::{ChatCompletionParams, ChatMessage, InferenceProvider, MessageRole};
        use std::sync::Arc;
        use tokio::net::TcpListener;

        // A listener that accepts TCP connections but never sends any HTTP
        // bytes back — every request times out at the configured cap.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accept_count_clone = accept_count.clone();
        let acceptor = tokio::spawn(async move {
            // Park each accepted socket on the task — when the test returns and
            // `acceptor` is aborted, sockets get dropped (and connections closed)
            // without the leak that `mem::forget` would cause.
            let mut held = Vec::new();
            loop {
                if let Ok((sock, _)) = listener.accept().await {
                    accept_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    held.push(sock);
                }
            }
        });

        struct DirectClient;
        #[async_trait::async_trait]
        impl crate::BackendVerifier for DirectClient {
            async fn create_verified_client(
                &self,
                _base_url: &str,
            ) -> Result<reqwest::Client, String> {
                Ok(reqwest::Client::builder()
                    .build()
                    .expect("client builds in test"))
            }
        }

        let provider = Provider::new_with_verifier(
            Config {
                base_url: format!("http://{addr}"),
                api_key: None,
                completion_timeout_seconds: 1,
                control_timeout_seconds: 30,
            },
            Arc::new(std::sync::RwLock::new(
                crate::spki_verifier::FingerprintState::Bootstrap,
            )),
            Arc::new(DirectClient),
        );

        let params = ChatCompletionParams {
            model: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::Value::String("hi".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_completion_tokens: Some(1),
            max_tokens: None,
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            frequency_penalty: None,
            presence_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: None,
            seed: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: None,
            store: None,
            stream_options: None,
            modalities: None,
            extra: std::collections::HashMap::new(),
        };

        let result = provider
            .chat_completion(params, "test-hash".to_string())
            .await;

        // Must surface as Timeout, not as a generic CompletionError.
        match result {
            Err(CompletionError::Timeout {
                operation,
                timeout_seconds,
            }) => {
                assert_eq!(operation, "chat_completion");
                assert_eq!(timeout_seconds, 1);
            }
            other => panic!("expected CompletionError::Timeout, got: {other:?}"),
        }

        // The regression guard, asserted deterministically: without the
        // `!is_timeout()` check, the timeout would fall into the bucket-clear
        // retry branch and open a *second* backend connection. Exactly one TCP
        // accept proves no retry fired — no wall-clock comparison, so this
        // cannot flake under test-harness CPU load.
        assert_eq!(
            accept_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one TCP connection should have been opened (no retry)"
        );

        // Drop the acceptor task: this releases the held sockets cleanly so
        // we don't leak file descriptors past the test.
        acceptor.abort();
    }

    /// pre_warm: spawns a background task per live backend index
    /// (`0..rotation_count()`) that calls get_or_verify_index_client. After
    /// awaiting all tasks, exactly those index slots should be filled and the
    /// verifier should have been called exactly once per index (the semaphore
    /// double-check prevents duplicate calls for the same index, but each index
    /// still needs its own client).
    #[tokio::test]
    async fn test_pre_warm_fills_live_index_clients() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        struct CountingVerifier {
            count: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl crate::BackendVerifier for CountingVerifier {
            async fn create_verified_client(
                &self,
                _base_url: &str,
            ) -> Result<reqwest::Client, String> {
                self.count.fetch_add(1, Ordering::SeqCst);
                Ok(reqwest::Client::new())
            }
        }

        let provider = Arc::new(Provider::new_with_verifier_and_concurrency(
            Config {
                // Rotation-capable URL so pre_warm can derive live indices.
                base_url: "https://glm-5-1.completions.near.ai".to_string(),
                api_key: None,
                completion_timeout_seconds: 30,
                control_timeout_seconds: 30,
            },
            Arc::new(std::sync::RwLock::new(
                // Need at least one pinned fingerprint so pre_warm doesn't
                // skip due to the Bootstrap/Blocked guard (pinned_count > 0).
                crate::spki_verifier::FingerprintState::Pinned(
                    std::iter::once("dummy-fp".to_string()).collect(),
                ),
            )),
            Arc::new(CountingVerifier {
                count: call_count_clone,
            }),
            4, // production-default semaphore concurrency — exercises throttling
        ));

        // Discovery reports a live backend count; pre_warm warms 0..count.
        use crate::InferenceProvider;
        let live_count = 5usize;
        provider.set_backend_count(live_count);
        assert_eq!(provider.fleet.rotation_count(), live_count);

        // All index slots start empty.
        assert!(provider
            .fleet
            .index_clients
            .iter()
            .all(|b| b.lock().unwrap().is_none()));

        // pre_warm fires background tasks — wait for the live indices to fill.
        provider.clone().pre_warm();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let filled = provider.fleet.index_clients[..live_count]
                .iter()
                .filter(|b| b.lock().unwrap().is_some())
                .count();
            if filled == live_count {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "pre_warm did not fill all {live_count} live index clients within timeout; filled={filled}"
            );
            tokio::task::yield_now().await;
        }

        // Exactly the live indices should be filled, and only the live indices —
        // slots past the live count must remain empty.
        assert!(
            provider.fleet.index_clients[live_count..]
                .iter()
                .all(|b| b.lock().unwrap().is_none()),
            "pre_warm must not warm index slots beyond the live count"
        );

        // The verifier should have been called exactly once per live index.
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            live_count,
            "expected one verification call per live index"
        );
    }

    /// pre_warm is a no-op when no backend verifier is configured (legacy mode).
    #[tokio::test]
    async fn test_pre_warm_noop_without_verifier() {
        let provider = Arc::new(Provider::new(Config {
            base_url: "http://localhost".to_string(),
            api_key: None,
            completion_timeout_seconds: 30,
            control_timeout_seconds: 30,
        }));

        // In legacy mode index clients are eagerly pre-filled at construction.
        assert!(provider
            .fleet
            .index_clients
            .iter()
            .all(|b| b.lock().unwrap().is_some()));

        // pre_warm should not panic and should not clear the pre-filled clients.
        provider.clone().pre_warm();
        assert!(provider
            .fleet
            .index_clients
            .iter()
            .all(|b| b.lock().unwrap().is_some()));
    }

    /// pre_warm is a no-op when no fingerprints are pinned (Bootstrap or Blocked state).
    /// Without this guard, pre_warm would spawn 64 tasks that each fail the security
    /// check in get_or_verify_index_client and log spurious warnings.
    #[tokio::test]
    async fn test_pre_warm_skips_without_pinned_fingerprints() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        struct CountingVerifier {
            count: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl crate::BackendVerifier for CountingVerifier {
            async fn create_verified_client(
                &self,
                _base_url: &str,
            ) -> Result<reqwest::Client, String> {
                self.count.fetch_add(1, Ordering::SeqCst);
                Ok(reqwest::Client::new())
            }
        }

        for state in [
            crate::spki_verifier::FingerprintState::Bootstrap,
            crate::spki_verifier::FingerprintState::Blocked,
        ] {
            let call_count = Arc::new(AtomicUsize::new(0));
            let provider = Arc::new(Provider::new_with_verifier_and_concurrency(
                Config {
                    // Rotation-capable URL + a live count, so the only thing
                    // stopping pre_warm is the fingerprint guard (not count==0).
                    base_url: "https://glm-5-1.completions.near.ai".to_string(),
                    api_key: None,
                    completion_timeout_seconds: 30,
                    control_timeout_seconds: 30,
                },
                Arc::new(std::sync::RwLock::new(state)),
                Arc::new(CountingVerifier {
                    count: call_count.clone(),
                }),
                4,
            ));
            use crate::InferenceProvider;
            provider.set_backend_count(5);

            // pre_warm must not spawn any tasks when no fingerprints are pinned.
            provider.clone().pre_warm();

            // Yield to let any spuriously-spawned tasks run.
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }

            assert_eq!(
                call_count.load(Ordering::SeqCst),
                0,
                "pre_warm should not call the verifier in Bootstrap/Blocked state"
            );
            // All index slots must remain empty (no tasks ran).
            assert!(
                provider
                    .fleet
                    .index_clients
                    .iter()
                    .all(|b| b.lock().unwrap().is_none()),
                "pre_warm should not fill any index clients in Bootstrap/Blocked state"
            );
        }
    }

    #[test]
    fn rotation_retryable_status_covers_5xx_429_and_408() {
        // Mirrors `classify_retry_decision` in the pool ("retryable_http_5xx"
        // + 429 + 408). Keeping these in sync is load-bearing: if the
        // rotation gate diverges, a 503 that the pool considers retryable
        // could bypass rotation and burn the pool's 3-round backoff against
        // the same overloaded bucket. 408 is included because the pool
        // already treats it as next-provider-worthy in the chat_completion
        // closure — and other indices may succeed where the sticky bucket
        // timed out.
        assert!(Fleet::is_rotation_retryable_status(408));
        assert!(Fleet::is_rotation_retryable_status(429));
        assert!(Fleet::is_rotation_retryable_status(500));
        assert!(Fleet::is_rotation_retryable_status(503));
        assert!(Fleet::is_rotation_retryable_status(599));
        assert!(!Fleet::is_rotation_retryable_status(200));
        assert!(!Fleet::is_rotation_retryable_status(400));
        assert!(!Fleet::is_rotation_retryable_status(401));
        assert!(!Fleet::is_rotation_retryable_status(404));
        assert!(!Fleet::is_rotation_retryable_status(422));
    }

    #[test]
    fn merge_model_responses_uses_max_metadata_across_backends() {
        let merged = merge_model_responses(vec![
            ModelsResponse {
                object: "list".to_string(),
                data: vec![
                    ModelInfo {
                        id: "test/model".to_string(),
                        object: "model".to_string(),
                        created: 1,
                        owned_by: "vllm".to_string(),
                        context_length: Some(8_192),
                        max_model_len: None,
                        max_output_length: Some(0),
                        top_provider: Some(TopProviderInfo {
                            context_length: Some(16_384),
                            max_completion_tokens: Some(-1),
                        }),
                    },
                    ModelInfo {
                        id: "nested-only/model".to_string(),
                        object: "model".to_string(),
                        created: 1,
                        owned_by: "vllm".to_string(),
                        context_length: None,
                        max_model_len: None,
                        max_output_length: Some(-2),
                        top_provider: None,
                    },
                ],
            },
            ModelsResponse {
                object: "list".to_string(),
                data: vec![
                    ModelInfo {
                        id: "test/model".to_string(),
                        object: "model".to_string(),
                        created: 1,
                        owned_by: "vllm".to_string(),
                        context_length: Some(32_768),
                        max_model_len: None,
                        max_output_length: Some(1_024),
                        top_provider: Some(TopProviderInfo {
                            context_length: Some(65_536),
                            max_completion_tokens: Some(4_096),
                        }),
                    },
                    ModelInfo {
                        id: "nested-only/model".to_string(),
                        object: "model".to_string(),
                        created: 1,
                        owned_by: "vllm".to_string(),
                        context_length: None,
                        max_model_len: None,
                        max_output_length: None,
                        top_provider: Some(TopProviderInfo {
                            context_length: None,
                            max_completion_tokens: Some(2_048),
                        }),
                    },
                ],
            },
        ]);

        assert_eq!(merged.data.len(), 2);
        assert_eq!(merged.data[0].context_length, Some(65_536));
        assert_eq!(merged.data[0].max_output_length, Some(4_096));
        assert_eq!(
            merged.data[0]
                .top_provider
                .as_ref()
                .and_then(|provider| provider.context_length),
            Some(65_536)
        );
        assert_eq!(
            merged.data[0]
                .top_provider
                .as_ref()
                .and_then(|provider| provider.max_completion_tokens),
            Some(4_096)
        );
        assert_eq!(merged.data[1].max_output_length, Some(2_048));
        assert!(merged.data[1].top_provider.is_none());
    }

    #[test]
    fn rotation_disabled_for_non_rotation_url() {
        // `localhost` is one-label → `split_inference_url` returns `None`, so
        // `rotation_parts` stays `None`, `rotation_count()` is forced to 0
        // even if discovery somehow wrote a non-zero count, and the
        // canonical-SNI 5xx propagates unchanged.
        let provider = create_test_provider();
        provider.set_backend_count(5);
        assert_eq!(
            provider.fleet.rotation_count(),
            0,
            "rotation must stay disabled for URLs that don't fit the <canonical>.<multi-label-base> shape"
        );
        assert!(provider
            .fleet
            .rotation_url(0, "/v1/chat/completions")
            .is_none());
    }

    #[test]
    fn rotation_url_uses_canonical_label_and_index() {
        let provider = Provider::new(Config {
            base_url: "https://glm-5-1.completions.near.ai".to_string(),
            api_key: None,
            completion_timeout_seconds: 30,
            control_timeout_seconds: 30,
        });
        provider.set_backend_count(3);
        assert_eq!(provider.fleet.rotation_count(), 3);
        let url0 = provider
            .fleet
            .rotation_url(0, "/v1/chat/completions")
            .expect("rotation URL build");
        let url2 = provider
            .fleet
            .rotation_url(2, "/v1/chat/completions")
            .expect("rotation URL build");
        assert_eq!(
            url0,
            "https://glm-5-1-i0.completions.near.ai/v1/chat/completions"
        );
        assert_eq!(
            url2,
            "https://glm-5-1-i2.completions.near.ai/v1/chat/completions"
        );
    }

    #[test]
    fn rotation_count_clamps_to_max_fanout() {
        // Defensive: a bogus `/backends/count` reading (race during deploy,
        // partial registry split) shouldn't let one 5xx burn unbounded
        // fresh-TCP handshakes. Mirrors the discovery path's cap.
        let provider = Provider::new(Config {
            base_url: "https://glm-5-1.completions.near.ai".to_string(),
            api_key: None,
            completion_timeout_seconds: 30,
            control_timeout_seconds: 30,
        });
        provider.set_backend_count(10_000);
        assert_eq!(provider.fleet.rotation_count(), crate::rotation::MAX_FANOUT);
    }

    #[test]
    fn rotation_count_returns_zero_when_discovery_has_not_run() {
        // First request after startup, before discovery's first cycle: count
        // is 0, so rotation is skipped and the canonical 5xx propagates
        // as it did pre-this-PR. No false positives.
        let provider = Provider::new(Config {
            base_url: "https://glm-5-1.completions.near.ai".to_string(),
            api_key: None,
            completion_timeout_seconds: 30,
            control_timeout_seconds: 30,
        });
        assert_eq!(provider.fleet.rotation_count(), 0);
    }

    // --- Index-addressed routing: selection, EMA, count-change reset. ---

    /// A rotation-capable provider (no verifier → legacy eager clients) with a
    /// live backend count set, so `select_index` / `rotation_count` are active.
    fn rotation_provider(count: usize) -> Provider {
        use crate::InferenceProvider;
        let provider = Provider::new(Config {
            base_url: "https://glm-5-1.completions.near.ai".to_string(),
            api_key: None,
            completion_timeout_seconds: 30,
            control_timeout_seconds: 30,
        });
        provider.set_backend_count(count);
        provider
    }

    fn user_msg(content: &str) -> crate::ChatMessage {
        crate::ChatMessage {
            role: crate::MessageRole::User,
            content: Some(serde_json::Value::String(content.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    #[test]
    fn select_index_returns_none_when_rotation_disabled() {
        // localhost → no rotation parts → rotation_count()==0 → canonical
        // fallback path. Many tests (and cold-start) depend on this.
        let provider = create_test_provider();
        let msgs = vec![user_msg("hello")];
        assert_eq!(provider.fleet.select_index(&msgs), None);
    }

    #[test]
    fn select_index_is_prefix_hash_mod_count_with_no_stats() {
        // With no TTFT samples recorded, selection is pure prefix affinity:
        // `prefix_router.route(messages) % count`.
        let provider = rotation_provider(8);
        let msgs = vec![user_msg("a stable system prompt")];
        let expected = provider.fleet.prefix_router.route(&msgs) % 8;
        let got = provider.fleet.select_index(&msgs).expect("rotation active");
        assert_eq!(got, expected);
        // Deterministic / stable across calls (same prefix → same backend).
        assert_eq!(provider.fleet.select_index(&msgs), Some(expected));
    }

    #[test]
    fn select_index_steers_off_pathologically_slow_preferred_backend() {
        let provider = rotation_provider(4);
        let msgs = vec![user_msg("route me")];
        let preferred = provider.fleet.prefix_router.route(&msgs) % 4;

        // Warm the preferred backend as pathologically slow (>floor and >2× the
        // fastest), and a different backend as fast. Pick the fast index to be
        // distinct from `preferred`.
        let fast = (preferred + 1) % 4;
        for _ in 0..super::fleet::TTFT_WARMUP_SAMPLES {
            provider.fleet.record_ttft(preferred, 2000.0);
            provider.fleet.record_ttft(fast, 100.0);
        }

        let got = provider.fleet.select_index(&msgs).expect("rotation active");
        assert_eq!(
            got, fast,
            "should steer from the slow preferred backend to the fastest warmed one"
        );

        // Below the slow ratio (only ~1.5× the fast peer) → keep affinity.
        let provider2 = rotation_provider(4);
        let preferred2 = provider2.fleet.prefix_router.route(&msgs) % 4;
        let other2 = (preferred2 + 1) % 4;
        for _ in 0..super::fleet::TTFT_WARMUP_SAMPLES {
            provider2.fleet.record_ttft(preferred2, 900.0);
            provider2.fleet.record_ttft(other2, 600.0);
        }
        assert_eq!(
            provider2.fleet.select_index(&msgs),
            Some(preferred2),
            "below the 2x slow ratio, prefix affinity wins"
        );
    }

    #[test]
    fn record_ttft_ema_warmup_then_stable() {
        let provider = rotation_provider(4);
        // First sample seeds the EMA exactly.
        provider.fleet.record_ttft(0, 100.0);
        {
            let stats = provider.fleet.backend_stats.lock().unwrap();
            assert_eq!(stats[0].ttft_ewma_ms, 100.0);
            assert_eq!(stats[0].samples, 1);
        }
        // Warmup alpha = 0.5: 0.5*200 + 0.5*100 = 150.
        provider.fleet.record_ttft(0, 200.0);
        {
            let stats = provider.fleet.backend_stats.lock().unwrap();
            assert!((stats[0].ttft_ewma_ms - 150.0).abs() < 1e-9);
            assert_eq!(stats[0].samples, 2);
        }
        // Drive past the warmup threshold so the stable alpha (0.1) applies.
        for _ in 0..super::fleet::TTFT_WARMUP_SAMPLES {
            provider.fleet.record_ttft(0, 150.0);
        }
        let before = provider.fleet.backend_stats.lock().unwrap()[0].ttft_ewma_ms;
        // Stable alpha = 0.1: a big spike moves the EMA only ~10% toward it.
        provider.fleet.record_ttft(0, 1150.0);
        let after = provider.fleet.backend_stats.lock().unwrap()[0].ttft_ewma_ms;
        let expected = 0.1 * 1150.0 + 0.9 * before;
        assert!(
            (after - expected).abs() < 1e-6,
            "stable EMA step mismatch: after={after}, expected={expected}"
        );
        // Non-positive samples are ignored.
        let s = provider.fleet.backend_stats.lock().unwrap()[0].samples;
        provider.fleet.record_ttft(0, 0.0);
        provider.fleet.record_ttft(0, -5.0);
        assert_eq!(provider.fleet.backend_stats.lock().unwrap()[0].samples, s);
    }

    #[test]
    fn store_backend_count_change_clears_clients_and_resets_stats() {
        use crate::InferenceProvider;
        use std::sync::Arc;
        // Verifier mode (the production path): a count CHANGE must drop the
        // pinned index clients — the index↔backend binding is only stable while
        // the count is — so each `None` slot is lazily re-verified against the
        // new mapping. It also resets the per-index EMA. Seed a count, pin a
        // client + EMA on index 0, then observe the clear/reset on the change.
        struct NoopVerifier;
        #[async_trait::async_trait]
        impl crate::BackendVerifier for NoopVerifier {
            async fn create_verified_client(
                &self,
                _base_url: &str,
            ) -> Result<reqwest::Client, String> {
                Ok(reqwest::Client::new())
            }
        }
        let provider = Provider::new_with_verifier(
            Config {
                base_url: "https://glm-5-1.completions.near.ai".to_string(),
                api_key: None,
                completion_timeout_seconds: 30,
                control_timeout_seconds: 30,
            },
            Arc::new(std::sync::RwLock::new(
                crate::spki_verifier::FingerprintState::Bootstrap,
            )),
            Arc::new(NoopVerifier),
        );
        provider.set_backend_count(4);
        *provider.fleet.index_clients[0].lock().unwrap() = Some(reqwest::Client::new());
        for _ in 0..super::fleet::TTFT_WARMUP_SAMPLES {
            provider.fleet.record_ttft(0, 123.0);
        }

        // Same count → no reset (clients + stats preserved).
        provider.set_backend_count(4);
        assert!(provider.fleet.index_clients[0].lock().unwrap().is_some());
        assert!(provider.fleet.backend_stats.lock().unwrap()[0].samples > 0);

        // Changed count → clients cleared + stats reset.
        provider.set_backend_count(6);
        assert!(
            provider.fleet.index_clients[0].lock().unwrap().is_none(),
            "count change must clear pinned index clients in verifier mode"
        );
        let stats = provider.fleet.backend_stats.lock().unwrap();
        assert!(
            stats
                .iter()
                .all(|s| s.samples == 0 && s.ttft_ewma_ms == 0.0),
            "count change must reset all backend stats"
        );
    }

    #[test]
    fn store_backend_count_change_keeps_legacy_clients_but_resets_stats() {
        use crate::InferenceProvider;
        // Legacy/no-verifier mode: index clients are eagerly pre-created and
        // there is no verifier to lazily re-create them, so a count change must
        // NOT clear them (clearing would wedge the provider with "no backend
        // verifier configured"). Stats are still reset.
        let provider = rotation_provider(4); // Provider::new → no verifier
        for _ in 0..super::fleet::TTFT_WARMUP_SAMPLES {
            provider.fleet.record_ttft(0, 123.0);
        }
        assert!(provider.fleet.index_clients[0].lock().unwrap().is_some());

        provider.set_backend_count(6);
        assert!(
            provider.fleet.index_clients[0].lock().unwrap().is_some(),
            "legacy eager clients must survive a count change (no verifier to rebuild them)"
        );
        let stats = provider.fleet.backend_stats.lock().unwrap();
        assert!(
            stats
                .iter()
                .all(|s| s.samples == 0 && s.ttft_ewma_ms == 0.0),
            "count change must still reset all backend stats in legacy mode"
        );
    }

    #[test]
    fn fallback_indices_orders_warmed_fastest_first_and_skips_tried() {
        let provider = rotation_provider(4);
        // Warm indices 1 (slow) and 3 (fast); leave 0 and 2 unwarmed.
        for _ in 0..super::fleet::TTFT_WARMUP_SAMPLES {
            provider.fleet.record_ttft(1, 800.0);
            provider.fleet.record_ttft(3, 120.0);
        }
        let order = provider.fleet.fallback_indices(0);
        assert!(!order.contains(&0), "tried index must be skipped");
        // Warmed fastest first (3 before 1), then unwarmed (just 2 here).
        assert_eq!(order, vec![3, 1, 2]);
    }

    #[tokio::test]
    async fn ttft_probe_records_on_first_content_chunk_only() {
        use std::sync::{Arc, Mutex};
        use tokio_stream::StreamExt;

        let stats = Arc::new(Mutex::new(vec![
            super::fleet::BackendStat::default();
            crate::rotation::MAX_FANOUT
        ]));
        // Leading control event (no chunk) → must NOT record; first data chunk
        // → records exactly one sample; subsequent data chunk → no new sample.
        let items: Vec<Result<SSEEvent, CompletionError>> = vec![
            Ok(control_event(": keepalive\n")),
            Ok(data_event()),
            Ok(data_event()),
        ];
        let inner: StreamingResult = Box::pin(futures_util::stream::iter(items));
        let index = 2usize;
        // Start a few ms in the past so the measured TTFT is strictly positive
        // (a 0ms reading is dropped by the EMA guard) — deterministic regardless
        // of how fast the test polls.
        let start = std::time::Instant::now() - std::time::Duration::from_millis(5);
        let probe = TtftProbe::new(inner, stats.clone(), index, start);
        tokio::pin!(probe);
        let mut count = 0;
        while let Some(_ev) = probe.next().await {
            count += 1;
        }
        assert_eq!(count, 3, "all events must pass through unchanged");
        let s = stats.lock().unwrap()[index];
        assert_eq!(
            s.samples, 1,
            "exactly one TTFT sample recorded on the first content chunk"
        );
        assert!(s.ttft_ewma_ms >= 0.0);
    }

    #[test]
    fn pin_chat_connection_promotes_pending_rotation_to_signature_rotation() {
        // The streaming fallback stores `request_hash → index` in
        // `pending_rotation` because the chat_id isn't known at send time.
        // Once the first chunk yields a chat_id, `pin_chat_connection`
        // must promote that mapping into `signature_rotation` so the
        // signature fetch reuses the same rotation index. Without this
        // promotion the signature endpoint would land on the LB-chosen
        // backend and 404.
        let provider = create_test_provider();
        provider
            .fleet
            .pending_rotation
            .lock()
            .unwrap()
            .insert("req-hash-abc".to_string(), 2);
        provider.pin_chat_connection("req-hash-abc", "chatcmpl-xyz");
        let stored = provider
            .fleet
            .signature_rotation
            .lock()
            .unwrap()
            .get("chatcmpl-xyz")
            .copied();
        assert_eq!(stored, Some(2));
        // Pending entry should be drained so a future `request_hash` reuse
        // can't accidentally surface the stale index.
        assert!(provider.fleet.pending_rotation.lock().unwrap().is_empty());
    }

    #[test]
    fn pin_chat_connection_with_empty_chat_id_drains_pending_without_writing_signature() {
        // The pool's orphan-cleanup path (`provider.pin_chat_connection(hash, "")`)
        // must drop the pending mapping without leaking an entry under an
        // empty chat_id key — otherwise every orphan request would
        // collide on the same `""` signature_rotation slot.
        let provider = create_test_provider();
        provider
            .fleet
            .pending_rotation
            .lock()
            .unwrap()
            .insert("req-hash-orphan".to_string(), 1);
        provider.pin_chat_connection("req-hash-orphan", "");
        assert!(provider.fleet.pending_rotation.lock().unwrap().is_empty());
        assert!(provider.fleet.signature_rotation.lock().unwrap().is_empty());
    }

    #[test]
    fn unpin_chat_connection_clears_signature_rotation() {
        let provider = create_test_provider();
        provider
            .fleet
            .signature_rotation
            .lock()
            .unwrap()
            .insert("chat-1".to_string(), 4);
        provider.unpin_chat_connection("chat-1");
        assert!(provider.fleet.signature_rotation.lock().unwrap().is_empty());
    }

    // --- Characterization tests for get_signature's fetch/retry behavior over
    // a real (mock) HTTP backend. With an IP-literal base_url the rotation path
    // is disabled, so these exercise the general-client walk + the 404 (signing
    // race) retry. They pin the network-facing contract the Fleet
    // extraction must preserve. ---

    /// Spawn a mock HTTP/1.1 backend. Each incoming request is answered with the
    /// status at `script[request_index]` (saturating at the last entry); a 200
    /// carries a valid `ChatSignature` JSON body. Returns the address, the
    /// acceptor handle (abort to stop), and a counter of requests served.
    async fn spawn_signature_mock(
        script: Vec<u16>,
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_acc = counter.clone();
        let handle = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(c) => c,
                    Err(_) => break,
                };
                let script = script.clone();
                let counter_conn = counter_acc.clone();
                tokio::spawn(async move {
                    // Read request headers (until CRLFCRLF); we don't need the body.
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    loop {
                        match sock.read(&mut tmp).await {
                            Ok(0) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                    let idx = counter_conn.fetch_add(1, Ordering::SeqCst);
                    let status = *script.get(idx).or_else(|| script.last()).unwrap_or(&404);
                    let resp = if status == 200 {
                        let body = serde_json::json!({
                            "text": "req:resp",
                            "signature": "0xsig",
                            "signing_address": "0xabc",
                            "signing_algo": "ecdsa",
                        })
                        .to_string();
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    } else {
                        format!("HTTP/1.1 {status} ERR\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    };
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        (addr, handle, counter)
    }

    fn mock_provider(addr: std::net::SocketAddr) -> Provider {
        Provider::new(Config {
            base_url: format!("http://{addr}"),
            api_key: None,
            completion_timeout_seconds: 5,
            control_timeout_seconds: 5,
        })
    }

    #[tokio::test]
    async fn get_signature_returns_signature_on_200() {
        use crate::InferenceProvider;
        use std::sync::atomic::Ordering;
        let (addr, handle, counter) = spawn_signature_mock(vec![200]).await;
        let provider = mock_provider(addr);
        let sig = provider
            .get_signature("chat-1", Some("ecdsa".to_string()))
            .await
            .expect("200 should yield a signature");
        assert_eq!(sig.signing_algo, "ecdsa");
        assert_eq!(sig.signing_address, "0xabc");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "exactly one fetch");
        handle.abort();
    }

    #[tokio::test]
    async fn get_signature_retries_on_404_then_succeeds() {
        use crate::InferenceProvider;
        use std::sync::atomic::Ordering;
        // 404 is the signing-race signal: the first fetch misses, the retry hits.
        let (addr, handle, counter) = spawn_signature_mock(vec![404, 200]).await;
        let provider = mock_provider(addr);
        let sig = provider
            .get_signature("chat-1", Some("ecdsa".to_string()))
            .await
            .expect("404 then 200 should succeed on retry");
        assert_eq!(sig.signing_algo, "ecdsa");
        assert_eq!(counter.load(Ordering::SeqCst), 2, "one 404, then a retry");
        handle.abort();
    }

    #[tokio::test]
    async fn get_signature_persistent_404_fails_after_bounded_retries() {
        use crate::InferenceProvider;
        use std::sync::atomic::Ordering;
        // Always 404: the fetch must give up after a bounded number of attempts
        // (1 initial + one per backoff in the schedule), not loop forever.
        let (addr, handle, counter) = spawn_signature_mock(vec![404]).await;
        let provider = mock_provider(addr);
        let res = provider
            .get_signature("chat-1", Some("ecdsa".to_string()))
            .await;
        assert!(res.is_err(), "persistent 404 is a definitive failure");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1 + super::SIGNATURE_FETCH_BACKOFFS_MS.len(),
            "1 initial fetch + one retry per backoff entry"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn get_attestation_report_delegates_to_fleet_without_recursing() {
        use crate::InferenceProvider;
        // Regression guard for the Provider -> Fleet delegation: the
        // trait method must forward to self.fleet, not self (which would resolve
        // back to the same trait method and recurse to a stack overflow). The
        // provider points at http://localhost with no server, so this returns a
        // transport error quickly — the point is that it RETURNS, not overflows.
        let provider = create_test_provider();
        let res = provider
            .get_attestation_report("test-model".to_string(), None, None, None, false)
            .await;
        assert!(
            res.is_err(),
            "expected a transport error (no backend), not a value or a stack overflow"
        );
    }

    #[test]
    fn signature_fetch_backoff_is_bounded_and_terminates() {
        // The signature-fetch retry runs in the hot path before `[DONE]`, so
        // it must add only a small, bounded delay and always terminate.
        // Index 0 is the wait before the 2nd attempt; the schedule yields
        // `len + 1` total attempts and then `None`.
        let n = super::SIGNATURE_FETCH_BACKOFFS_MS.len();
        assert!(n >= 1, "must retry at least once");

        // Retries terminate: no backoff at or beyond the final attempt index.
        assert!(super::signature_fetch_backoff(n).is_none());
        assert!(super::signature_fetch_backoff(n + 5).is_none());

        // Each scheduled retry yields a positive, sane delay.
        for i in 0..n {
            let d = super::signature_fetch_backoff(i).expect("backoff present");
            assert!(d > std::time::Duration::ZERO);
            assert!(d <= std::time::Duration::from_secs(1));
        }

        // Total added latency stays comfortably under the caller's 5s
        // FINALIZE_TIMEOUT budget.
        let total_ms: u64 = super::SIGNATURE_FETCH_BACKOFFS_MS.iter().sum();
        assert!(
            total_ms < 2_000,
            "total backoff {total_ms}ms must stay well under FINALIZE_TIMEOUT"
        );
    }
}
