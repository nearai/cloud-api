mod fleet;
mod placement_report;
mod prefix_router;
#[cfg(test)]
mod systemone_tests;

use crate::spki_verifier::{FingerprintState, SharedTlsRoots};
use crate::{
    models::StreamOptions, sse_parser::new_sse_parser, ImageEditError, ImageGenerationError,
    PrivacyClassifyError, RerankError, ScoreError, *,
};
use async_trait::async_trait;
use fleet::Fleet;
use placement_report::PlacementRequest;
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

/// HTTP headers cloud-api itself sets on an upstream request, never taken
/// from client input.
pub mod upstream_headers {
    /// The replica index placement chose on the target backend, in decimal.
    /// Set only from the `RouteLease`, and only on the request sent to that
    /// lease's backend; a key of the same name in `params.extra` is dropped.
    pub const REPLICA_HINT: &str = "x-nearai-replica";
    /// The host id placement chose, sent with [`REPLICA_HINT`]. The rotation
    /// index can drift to another host before the next discovery (the proxy
    /// maps `-i<N>` over its live healthy set), so the proxy honours the hint
    /// only when this matches its own host. Same rules as the hint: set only
    /// from the `RouteLease`, and a key of this name in `params.extra` is
    /// dropped.
    pub const REPLICA_HINT_HOST: &str = "x-nearai-replica-host";

    /// Every header above: none may come from client input.
    pub(crate) const ALL: [&str; 2] = [REPLICA_HINT, REPLICA_HINT_HOST];
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
    pub const MODEL_PUB_KEY: &str = "x_model_pub_key";
    /// Key for encryption version (x-encryption-version header)
    pub const ENCRYPTION_VERSION: &str = "x_encryption_version";
    /// Key for full field encryption opt-in (x-encrypt-all-fields header)
    pub const ENCRYPT_ALL_FIELDS: &str = "x_encrypt_all_fields";
}

/// Legacy placement keys in `params.extra`. Placement inputs now travel on
/// the typed, never-serialized `ChatCompletionParams::placement`, so nothing
/// writes or reads these; they stay on a deny-list so a client-supplied value
/// is still stripped before any upstream (this provider, Chutes, external).
pub mod placement_headers {
    /// The old affinity-key and affinity-source extra keys.
    pub const LEGACY_DENIED_EXTRA_KEYS: [&str; 2] =
        ["x_placement_affinity", "x_placement_affinity_source"];
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

    /// How long an index is not verified again after inline verification
    /// failed the TLS channel-binding check (a non-retryable failure). Bounds
    /// the attestation load during a certificate-renewal skew, when every
    /// request to the index would otherwise trigger a new attestation.
    pub(super) const CHANNEL_BINDING_BACKOFF: Duration = Duration::from_secs(30);

    /// Time left before `index` may be verified again after a channel-binding
    /// failure, if any.
    fn channel_binding_backoff_remaining(&self, index: usize) -> Option<Duration> {
        let failed_at = (*self.channel_binding_failed_at[index]
            .lock()
            .unwrap_or_else(|e| e.into_inner()))?;
        Self::CHANNEL_BINDING_BACKOFF
            .checked_sub(failed_at.elapsed())
            .filter(|remaining| !remaining.is_zero())
    }

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
    /// pinned (else fails closed). A TLS channel-binding failure is not
    /// retried, and the index is not verified again for
    /// `CHANNEL_BINDING_BACKOFF`.
    pub(super) async fn get_or_verify_index_client(
        &self,
        index: usize,
    ) -> Result<Client, CompletionError> {
        // Defensive bound. By construction every caller derives `index` from
        // `select_index`/`fallback_indices_for` (both bounded by `rotation_count()`
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

        if let Some(remaining) = self.channel_binding_backoff_remaining(index) {
            return self.channel_binding_backoff(index, remaining);
        }

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
        // A concurrent verification of this index may have just failed the
        // channel-binding check while this task waited for the permit.
        if let Some(remaining) = self.channel_binding_backoff_remaining(index) {
            return self.channel_binding_backoff(index, remaining);
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
        let mut attempts = 0;
        for _attempt in 0..=Self::INLINE_VERIFY_RETRIES {
            attempts += 1;
            match verifier.create_verified_client(&verify_url).await {
                Ok(client) => {
                    let mut guard = self.index_clients[index]
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(ref existing) = *guard {
                        return Ok(existing.clone());
                    }
                    *guard = Some(client.clone());
                    *self.channel_binding_failed_at[index]
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = None;
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
                    if matches!(e, BackendVerifyError::ChannelBinding(_)) {
                        // The report and the certificate came from the same
                        // connection: another attempt reaches the same backend
                        // and fails the same way, at the cost of a new
                        // attestation.
                        *self.channel_binding_failed_at[index]
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = Some(tokio::time::Instant::now());
                        tracing::warn!(
                            index,
                            error = %e,
                            backoff_secs = Self::CHANNEL_BINDING_BACKOFF.as_secs(),
                            "Inline backend verification failed the TLS channel binding check; not retrying"
                        );
                        last_err = Some(e);
                        break;
                    }
                    tracing::warn!(index, error = %e, "Inline backend verification failed, retrying");
                    last_err = Some(e);
                }
            }
        }

        // Retries exhausted, or a non-retryable failure. Fall back to the
        // non-pinned client ONLY if a fingerprint is already pinned (its
        // verifier still rejects unknown SPKIs); in Bootstrap, fail closed to
        // avoid unauthenticated connections.
        let err_msg = format!(
            "Inline backend verification failed after {attempts} attempt(s): {}",
            last_err.map(|e| e.to_string()).unwrap_or_default()
        );
        if self.pinned_fingerprint_count() > 0 {
            tracing::warn!(index, error = %err_msg, "Inline backend verification failed; serving with fallback client");
            Ok(self.fallback_client.clone())
        } else {
            tracing::warn!(
                index,
                error = %err_msg,
                "Inline backend verification failed in Bootstrap state; \
                 refusing fallback to prevent unauthenticated connections"
            );
            Err(CompletionError::CompletionError(err_msg))
        }
    }

    /// `get_or_verify_index_client` for an index that is backing off after a
    /// channel-binding failure: no verification, straight to the same fallback
    /// decision. Logged at debug level; the failure itself was logged once.
    fn channel_binding_backoff(
        &self,
        index: usize,
        remaining: Duration,
    ) -> Result<Client, CompletionError> {
        let err_msg = format!(
            "Inline backend verification of index {index} failed the TLS channel binding check; \
             not verifying it again for {}s",
            remaining.as_secs().max(1)
        );
        if self.pinned_fingerprint_count() > 0 {
            tracing::debug!(index, error = %err_msg, "Serving with fallback client");
            Ok(self.fallback_client.clone())
        } else {
            tracing::debug!(index, error = %err_msg, "Refusing fallback in Bootstrap state");
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

        // General-purpose client for non-completion requests. Like the fallback
        // and bucket clients, it does not follow redirects: every request goes
        // to a URL derived from `base_url`, and backends do not redirect.
        let client = Client::builder()
            .use_preconfigured_tls(tls_roots.build_config(fingerprint_state.clone()))
            .redirect(reqwest::redirect::Policy::none())
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
            .redirect(reqwest::redirect::Policy::none())
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
    /// Move encryption values out of `extra` and onto HTTP headers, returning the
    /// `x_model_pub_key` pin that was removed.
    ///
    /// The pin is deliberately not forwarded upstream because it is routing-only. Only
    /// chat paths consume the return value, using it with `acquire_index` and
    /// `fallback_indices_for` to select a backend holding that key. Other endpoints call
    /// this for its header side effect alone; they use the canonical SNI without
    /// backend-index affinity, so there is no pin for them to honour.
    fn prepare_encryption_headers(
        &self,
        headers: &mut reqwest::header::HeaderMap,
        extra: &mut std::collections::HashMap<String, serde_json::Value>,
    ) -> Option<String> {
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

        // Capture x_model_pub_key for backend affinity, but do not forward it
        // to vllm-proxy or leave it in the serialized request body.
        let pinned_pub_key = extra
            .remove(encryption_headers::MODEL_PUB_KEY)
            .and_then(|value| value.as_str().map(str::to_string));

        // Legacy placement keys are denied: a client-supplied value never
        // reaches the serialized request body or an HTTP header.
        for key in placement_headers::LEGACY_DENIED_EXTRA_KEYS {
            extra.remove(key);
        }

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

        pinned_pub_key
    }

    fn prepare_priority_header(
        headers: &mut reqwest::header::HeaderMap,
        params: &mut ChatCompletionParams,
    ) {
        headers.insert(
            "x-nearai-priority",
            HeaderValue::from(params.request_priority),
        );
        params.strip_client_priority();
    }

    /// `headers` plus the lease's replica hint and its host, when placement
    /// chose one. Only the request to the lease's own backend carries them: a
    /// fallback index or the canonical URL lands on a backend whose replicas
    /// the decision never saw. A host id that is not a valid header value
    /// sends neither, so the hint never goes out without its host.
    fn with_replica_hint(
        headers: &reqwest::header::HeaderMap,
        lease: &fleet::RouteLease,
    ) -> reqwest::header::HeaderMap {
        let mut headers = headers.clone();
        if let (Some(replica), Some(host)) = (lease.replica(), lease.replica_host()) {
            if let Ok(host) = HeaderValue::from_str(host) {
                headers.insert(upstream_headers::REPLICA_HINT, HeaderValue::from(replica));
                headers.insert(upstream_headers::REPLICA_HINT_HOST, host);
            }
        }
        headers
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
        route_key: u64,
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
            let _route_lease = self.reserve_index(route_key, index);
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
        route_key: u64,
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
            let route_lease = self.reserve_index(route_key, index);
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
                Some(route_lease),
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
    /// Send instant, kept for the end-of-stream duration.
    sent: std::time::Instant,
    /// First and last token chunk (at least one choice), and how many,
    /// for the inter-token latency.
    first_token: Option<std::time::Instant>,
    last_token: Option<std::time::Instant>,
    token_chunks: u64,
    /// Keeps this backend counted as live until the stream completes or the
    /// caller drops it. `None` is used only by focused unit tests.
    _route_lease: Option<fleet::RouteLease>,
}

impl<S> TtftProbe<S> {
    fn new(
        inner: S,
        stats: Arc<std::sync::Mutex<Vec<fleet::BackendStat>>>,
        index: usize,
        start: std::time::Instant,
        route_lease: Option<fleet::RouteLease>,
    ) -> Self {
        Self {
            inner,
            stats,
            index,
            start: Some(start),
            sent: start,
            first_token: None,
            last_token: None,
            token_chunks: 0,
            _route_lease: route_lease,
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
            if event.chunk.as_ref().is_some_and(is_token_chunk) {
                let now = std::time::Instant::now();
                self.first_token.get_or_insert(now);
                self.last_token = Some(now);
                self.token_chunks = self.token_chunks.saturating_add(1);
            }
            if event.chunk.is_some() {
                if let Some(start) = self.start.take() {
                    let ttft_ms = start.elapsed().as_millis() as f64;
                    if let Some(lease) = self._route_lease.as_ref() {
                        lease.record_ttft_ms(ttft_ms);
                    }
                    let index = self.index;
                    let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(s) = stats.get_mut(index) {
                        fleet::update_ema(s, ttft_ms);
                    }
                }
            }
        } else if matches!(polled, std::task::Poll::Ready(None)) {
            if let Some(lease) = self._route_lease.as_ref() {
                lease.record_duration_ms(self.sent.elapsed().as_millis() as f64);
                if let (Some(first), Some(last)) = (self.first_token, self.last_token) {
                    let span_ms = last.duration_since(first).as_secs_f64() * 1_000.0;
                    if let Some(itl) = fleet::mean_itl_ms(span_ms, self.token_chunks) {
                        lease.record_itl_ms(itl);
                    }
                }
            }
            self._route_lease.take();
        }
        polled
    }
}

/// A chunk that carries at least one choice, i.e. generated tokens. A
/// usage-only chunk (`choices: []`) is not one.
fn is_token_chunk(chunk: &StreamChunk) -> bool {
    match chunk {
        StreamChunk::Chat(c) => !c.choices.is_empty(),
        StreamChunk::Text(c) => !c.choices.is_empty(),
    }
}

#[async_trait]
impl InferenceProvider for Fleet {
    fn supports_systemone(&self) -> bool {
        true
    }

    async fn systemone(
        &self,
        request: SystemOneRequest,
        request_hash: String,
    ) -> Result<SystemOneResponseWithBytes, CompletionError> {
        let mut headers = self
            .build_headers()
            .map_err(CompletionError::CompletionError)?;
        headers.insert(
            "X-Request-Hash",
            HeaderValue::from_str(&request_hash)
                .map_err(|_| CompletionError::CompletionError("Invalid request hash".into()))?,
        );
        let mut lease = self.acquire_systemone_index(&request_hash);
        let route_key = lease.as_ref().map(|lease| lease.route_key());
        let indices = match &lease {
            Some(lease) => std::iter::once(lease.index())
                .chain(self.fallback_indices_for(lease.index(), None))
                .map(Some)
                .collect::<Vec<_>>(),
            None => vec![None],
        };
        let timeout_seconds = self.config.completion_timeout_seconds.max(1) as u64;
        let mut last_error = None;
        for index in indices {
            let _lease = lease.take().or_else(|| {
                index
                    .zip(route_key)
                    .map(|(index, key)| self.reserve_index(key, index))
            });
            let attempt = async {
                let (client, url) = match index {
                    Some(index) => (
                        self.get_or_verify_index_client(index).await?,
                        self.rotation_url(index as u64, "/v1/systemone")
                            .expect("rotation lease requires a rotation URL"),
                    ),
                    None => (
                        self.fallback_client.clone(),
                        format!("{}/v1/systemone", self.config.base_url),
                    ),
                };
                let response = client
                    .post(url)
                    .headers(headers.clone())
                    .json(&request)
                    .timeout(Duration::from_secs(timeout_seconds))
                    .send()
                    .await
                    .map_err(|e| crate::systemone::transport_error(e, timeout_seconds))?;
                let response = crate::systemone::read_response(response, &request, false).await?;
                let id = response.provider_signature_id()?;
                if let Some(index) = index {
                    self.signature_rotation
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(id.to_owned(), index as u64);
                }
                Ok(response)
            }
            .await;
            match attempt {
                Ok(response) => return Ok(response),
                Err(error) => {
                    // Never replay a successful but malformed response or an
                    // ambiguous read timeout. Those may already have incurred cost.
                    let retryable = match &error {
                        CompletionError::HttpError { status_code, .. } => {
                            Self::is_rotation_retryable_status(*status_code)
                        }
                        CompletionError::CompletionError(_) => true,
                        _ => false,
                    };
                    if !retryable {
                        return Err(error);
                    }
                    if let Some(index) = index {
                        if matches!(error, CompletionError::CompletionError(_)) {
                            self.clear_index(index);
                        }
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.expect("at least one System One backend was attempted"))
    }

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

    async fn poll_backend_count(&self, client: &reqwest::Client) -> crate::CountPoll {
        self.poll_count(client).await
    }

    fn count_generation(&self) -> u64 {
        self.current_count_generation()
    }

    fn apply_discovery(&self, push: crate::DiscoveryPush) -> bool {
        self.apply_discovery_push(push)
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

    /// Exact token count via the backend's `POST /v1/tokenize` passthrough
    /// (inference-proxy forwards it to the engine's native tokenize endpoint).
    ///
    /// Best-effort by design: any transport/HTTP/parse failure returns `None`
    /// and the caller falls back to its byte-based heuristic — this must never
    /// fail a request. Goes over `self.client`, whose TLS config enforces the
    /// pinned attested fingerprints, so the prompt text only ever reaches a
    /// verified backend. Nothing about the text (or the response body, which
    /// may echo token ids) is logged.
    async fn count_tokens(&self, model: &str, text: String) -> Option<u64> {
        // Tight on purpose: this sits on the request's critical path and is a
        // best-effort precision upgrade — a slow backend should fail-open to
        // the caller's heuristic, not add seconds of latency. (Tokenizing
        // ~1MB of text takes SGLang well under a second when healthy.)
        const TOKENIZE_TIMEOUT: Duration = Duration::from_secs(2);

        let url = format!("{}/v1/tokenize", self.config.base_url);
        let headers = self.build_headers().ok()?;
        let body = serde_json::json!({ "model": model, "prompt": text });

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(TOKENIZE_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::debug!(error = %e, "Tokenize request failed; falling back to heuristic");
            })
            .ok()?;

        if !response.status().is_success() {
            tracing::debug!(
                status = %response.status(),
                "Tokenize request returned non-2xx; falling back to heuristic"
            );
            return None;
        }

        let parsed: serde_json::Value = response.json().await.ok()?;
        // Engines differ slightly: vLLM/SGLang return `count` (int); fall back
        // to the length of a flat `tokens` array. Anything else → None.
        if let Some(count) = parsed.get("count").and_then(|c| c.as_u64()) {
            return Some(count);
        }
        parsed
            .get("tokens")
            .and_then(|t| t.as_array())
            .map(|arr| arr.len() as u64)
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

        Self::prepare_priority_header(&mut headers, &mut streaming_params);
        // The replica hint and its host are placement's choice alone: a
        // client-supplied key of either name never reaches the upstream body.
        for key in upstream_headers::ALL {
            streaming_params.extra.remove(key);
        }
        // Read placement inputs before the helpers below strip them.
        let placement_request = PlacementRequest::from_params(&streaming_params);
        // Prepare tracing headers (request_id, org_id, workspace_id)
        self.prepare_tracing_headers(&mut headers, &mut streaming_params.extra);
        // Prepare encryption headers
        let pinned_pub_key =
            self.prepare_encryption_headers(&mut headers, &mut streaming_params.extra);

        // Reserve a backend rotation index: deterministic prefix affinity keeps
        // initial requests cache-hot, established conversations stay on a
        // stable home, and TTFT steering excludes pathological backends.
        // `None` means rotation is unavailable (cold-start before discovery's
        // first count, or a non-rotation URL like `localhost`) — serve via the
        // canonical fallback path (no index, no per-backend measurement) so
        // those paths keep working unchanged.
        let route_lease = match self.acquire_index_placed(
            &streaming_params.messages,
            pinned_pub_key.as_deref(),
            &placement_request,
        )? {
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
            Some(lease) => lease,
        };
        let index = route_lease.index();
        let route_key = route_lease.route_key();

        // The index client maintains a persistent H2 connection to a verified
        // backend (`<canonical>-i<index>.<base>`) via L4 passthrough → prefix
        // cache hits. Index clients are lazily filled: on first use, inline
        // verification connects to the index's backend, verifies attestation,
        // and pins the client. Only this request carries the replica hint
        // (never the canonical URL or a fallback index).
        let rotation_url = self.rotation_url(index as u64, "/v1/chat/completions");
        let primary_headers = match rotation_url {
            Some(_) => Self::with_replica_hint(&headers, &route_lease),
            None => headers.clone(),
        };
        let url =
            rotation_url.unwrap_or_else(|| format!("{}/v1/chat/completions", self.config.base_url));
        let index_client = self.get_or_verify_index_client(index).await?;
        // Capture the send instant for the per-backend TTFT measurement.
        let started = std::time::Instant::now();
        let primary_send = match self
            .send_streaming_request(
                &url,
                primary_headers.clone(),
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
                self.send_streaming_request(&url, primary_headers, &streaming_params, Some(&fresh))
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
                            Some(route_lease),
                        ));
                        Ok(probed)
                    }
                    Some(status_code) => {
                        drop(stream);
                        drop(route_lease);
                        self.try_stream_fallback_indices(
                            &self.fallback_indices_for(index, pinned_pub_key.as_deref()),
                            route_key,
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
                    drop(route_lease);
                    self.try_stream_fallback_indices(
                        &self.fallback_indices_for(index, pinned_pub_key.as_deref()),
                        route_key,
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

        Self::prepare_priority_header(&mut headers, &mut non_streaming_params);
        // The replica hint and its host are placement's choice alone: a
        // client-supplied key of either name never reaches the upstream body.
        for key in upstream_headers::ALL {
            non_streaming_params.extra.remove(key);
        }
        // Read placement inputs before the helpers below strip them.
        let placement_request = PlacementRequest::from_params(&non_streaming_params);
        // Prepare tracing headers (request_id, org_id, workspace_id)
        self.prepare_tracing_headers(&mut headers, &mut non_streaming_params.extra);
        // Prepare encryption headers
        let pinned_pub_key =
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

        // Reserve the backend rotation index (deterministic first-turn prefix
        // affinity, stable conversation homes, and latency steering). `None`
        // → canonical fallback path (cold-start / non-rotation URL): one shot
        // via the non-pinned fallback client, no index recorded.
        let route_lease = match self.acquire_index_placed(
            &non_streaming_params.messages,
            pinned_pub_key.as_deref(),
            &placement_request,
        )? {
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
            Some(lease) => lease,
        };
        let index = route_lease.index();
        let route_key = route_lease.route_key();

        // Route to the index's verified client, posting at the index's rotation
        // SNI so completion + signature land on the same backend. Only this
        // request carries the replica hint (never the canonical URL or a
        // fallback index).
        let rotation_url = self.rotation_url(index as u64, "/v1/chat/completions");
        let primary_headers = match rotation_url {
            Some(_) => Self::with_replica_hint(&headers, &route_lease),
            None => headers.clone(),
        };
        let url =
            rotation_url.unwrap_or_else(|| format!("{}/v1/chat/completions", self.config.base_url));
        let index_client = self.get_or_verify_index_client(index).await?;

        let send = |client: &Client, hdrs: reqwest::header::HeaderMap| {
            client
                .post(&url)
                .headers(hdrs)
                .json(&non_streaming_params)
                .timeout(timeout)
                .send()
        };

        let response = match send(&index_client, primary_headers.clone()).await {
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
                send(&fresh, primary_headers).await.map_err(map_send_err)?
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
                drop(route_lease);
                return self
                    .try_chat_completion_fallback_indices(
                        &self.fallback_indices_for(index, pinned_pub_key.as_deref()),
                        route_key,
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
    fn supports_systemone(&self) -> bool {
        true
    }

    async fn systemone(
        &self,
        request: SystemOneRequest,
        request_hash: String,
    ) -> Result<SystemOneResponseWithBytes, CompletionError> {
        self.fleet.systemone(request, request_hash).await
    }

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
    fn set_backend_keys(&self, map: std::collections::HashMap<String, Vec<usize>>) {
        self.fleet.set_backend_keys(map)
    }
    fn set_backend_hosts(&self, hosts: crate::BackendHosts) {
        self.fleet.set_backend_hosts(hosts)
    }
    async fn poll_backend_count(&self, client: &reqwest::Client) -> crate::CountPoll {
        self.fleet.poll_backend_count(client).await
    }
    fn count_generation(&self) -> u64 {
        self.fleet.count_generation()
    }
    fn apply_discovery(&self, push: crate::DiscoveryPush) -> bool {
        self.fleet.apply_discovery(push)
    }
    fn set_placement(&self, handles: crate::placement_io::PlacementHandles) {
        self.fleet.set_placement(handles)
    }
    fn placement_tier(&self) -> Option<placement::policy::Tier> {
        self.fleet.placement_tier()
    }
    async fn count_tokens(&self, model: &str, text: String) -> Option<u64> {
        self.fleet.count_tokens(model, text).await
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
                service_tier: None,
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

        let pinned_pub_key = provider
            .fleet
            .prepare_encryption_headers(&mut headers, &mut extra);

        assert_eq!(pinned_pub_key.as_deref(), Some("def456"));

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

        let _ = provider
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

        let _ = provider
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
        let _ = provider
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

    /// Regression test: the legacy placement affinity keys
    /// (`x_placement_affinity`, `x_placement_affinity_source`) are denied,
    /// like `x_model_pub_key`: a client-supplied value never reaches the
    /// serialized upstream request body.
    #[test]
    fn test_placement_affinity_keys_never_reach_upstream_body() {
        let provider = create_test_provider();

        let mut extra = std::collections::HashMap::new();
        for key in placement_headers::LEGACY_DENIED_EXTRA_KEYS {
            extra.insert(
                key.to_string(),
                serde_json::Value::String("00112233445566778899aabbccddeeff".to_string()),
            );
        }
        extra.insert(
            "some_valid_param".to_string(),
            serde_json::Value::String("value".to_string()),
        );

        let mut headers = reqwest::header::HeaderMap::new();
        let _ = provider
            .fleet
            .prepare_encryption_headers(&mut headers, &mut extra);

        for key in placement_headers::LEGACY_DENIED_EXTRA_KEYS {
            assert!(!extra.contains_key(key), "{key} must be stripped");
        }

        // Non-affinity extra fields must be preserved.
        assert_eq!(
            extra.get("some_valid_param"),
            Some(&serde_json::Value::String("value".to_string()))
        );

        // No affinity-related HTTP header should have been added either.
        assert!(headers.get("X-Placement-Affinity").is_none());
    }

    /// End-to-end regression: a forged/leftover `x_placement_affinity` /
    /// `x_placement_affinity_source` in `params.extra` must never appear in
    /// the actual bytes sent to the upstream vLLM backend. Unlike
    /// `test_placement_affinity_keys_never_reach_upstream_body` above (which
    /// checks `prepare_encryption_headers` in isolation), this drives a real
    /// `ChatCompletionParams` through `Fleet::chat_completion`'s send path
    /// against a mock HTTP server and inspects the exact request body that
    /// left the process — the same kind of check as
    /// `audio_transcription_sends_repeated_timestamp_granularity_fields`.
    #[tokio::test]
    async fn chat_completion_never_sends_placement_affinity_keys_upstream() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": 0,
                "model": "test-model",
                "choices": [],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = Provider::new(Config::new(server.uri(), None, Some(5)));

        let mut params: ChatCompletionParams = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .unwrap();
        for key in placement_headers::LEGACY_DENIED_EXTRA_KEYS {
            params.extra.insert(
                key.to_string(),
                serde_json::Value::String("00112233445566778899aabbccddeeff".to_string()),
            );
        }
        // The typed channel carries a real key; it is never serialized.
        params.placement = crate::PlacementContext {
            prompt_tokens: Some(7),
            context_tokens: Some(9),
            heavy: true,
            prefill_heavy: true,
            affinity: Some(placement::affinity::AffinityKey::from_bytes([0x5a; 16])),
            affinity_source: placement::decision::AffinitySource::Client,
        };

        let result = provider
            .chat_completion(params, "test-hash".to_string())
            .await;
        assert!(
            result.is_ok(),
            "expected a successful completion, got: {:?}",
            result.err()
        );

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body = String::from_utf8_lossy(&requests[0].body);
        assert!(
            !body.contains("x_placement_affinity"),
            "placement affinity keys must never reach the upstream request body: {body}"
        );
        let json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(
            json.get("placement").is_none() && !body.contains(&"5a".repeat(16)),
            "the typed placement context must never be serialized upstream"
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
            ) -> Result<reqwest::Client, crate::BackendVerifyError> {
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
            ) -> Result<reqwest::Client, crate::BackendVerifyError> {
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
            ) -> Result<reqwest::Client, crate::BackendVerifyError> {
                Err("simulated attestation timeout".to_string().into())
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
            ) -> Result<reqwest::Client, crate::BackendVerifyError> {
                Err("simulated attestation timeout".to_string().into())
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
            ) -> Result<reqwest::Client, crate::BackendVerifyError> {
                Err("simulated attestation failure".to_string().into())
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

    /// Verifier that fails every call with the given error and counts calls.
    struct FailingVerifier {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        error: crate::BackendVerifyError,
    }

    #[async_trait::async_trait]
    impl crate::BackendVerifier for FailingVerifier {
        async fn create_verified_client(
            &self,
            _base_url: &str,
        ) -> Result<reqwest::Client, crate::BackendVerifyError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Yield so that concurrent callers queue on the permit.
            tokio::time::sleep(Duration::from_millis(10)).await;
            Err(self.error.clone())
        }
    }

    fn provider_with_failing_verifier(
        error: crate::BackendVerifyError,
        concurrency: usize,
    ) -> (Provider, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = Provider::new_with_verifier_and_concurrency(
            Config {
                base_url: "http://localhost".to_string(),
                api_key: None,
                completion_timeout_seconds: 30,
                control_timeout_seconds: 30,
            },
            std::sync::Arc::new(std::sync::RwLock::new(
                crate::spki_verifier::FingerprintState::Bootstrap,
            )),
            std::sync::Arc::new(FailingVerifier {
                calls: calls.clone(),
                error,
            }),
            concurrency,
        );
        (provider, calls)
    }

    fn channel_binding_mismatch() -> crate::BackendVerifyError {
        crate::BackendVerifyError::ChannelBinding(
            "TLS channel binding mismatch: peer SPKI aaaa, attested SPKI bbbb".to_string(),
        )
    }

    /// A channel-binding failure repeats for the same backend, so it is not
    /// retried, and the index is not verified again during the backoff: under a
    /// persistent mismatch, repeated requests cost one attestation per index per
    /// backoff period. The fallback decision is unchanged (fallback client once
    /// a fingerprint is pinned, fail closed before).
    #[tokio::test(start_paused = true)]
    async fn channel_binding_failure_is_not_retried_and_backs_off() {
        use std::sync::atomic::Ordering;
        for pinned in [false, true] {
            let (provider, calls) = provider_with_failing_verifier(channel_binding_mismatch(), 4);
            if pinned {
                provider.add_verified_fingerprint("deadbeef".to_string());
            }

            for _ in 0..5 {
                let result = provider.fleet.get_or_verify_index_client(0).await;
                assert_eq!(result.is_ok(), pinned, "pinned={pinned}: {result:?}");
                if let Err(CompletionError::CompletionError(msg)) = result {
                    assert!(msg.contains("TLS channel binding"), "{msg}");
                }
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1, "pinned={pinned}");
            assert!(provider.fleet.index_clients[0].lock().unwrap().is_none());

            // Other indices are verified independently.
            let _ = provider.fleet.get_or_verify_index_client(1).await;
            assert_eq!(calls.load(Ordering::SeqCst), 2, "pinned={pinned}");

            // After the backoff, the index is verified again.
            tokio::time::advance(Fleet::CHANNEL_BINDING_BACKOFF).await;
            let _ = provider.fleet.get_or_verify_index_client(0).await;
            assert_eq!(calls.load(Ordering::SeqCst), 3, "pinned={pinned}");

            // A backend-count change remaps indices and ends the backoff.
            let _ = provider.fleet.get_or_verify_index_client(0).await;
            assert_eq!(calls.load(Ordering::SeqCst), 3, "pinned={pinned}");
            provider.fleet.store_backend_count(2);
            let _ = provider.fleet.get_or_verify_index_client(0).await;
            assert_eq!(calls.load(Ordering::SeqCst), 4, "pinned={pinned}");
        }
    }

    /// Requests that queued on the verification permit while a channel-binding
    /// failure was being recorded do not verify the index again.
    #[tokio::test(start_paused = true)]
    async fn concurrent_requests_share_one_channel_binding_failure() {
        let (provider, calls) = provider_with_failing_verifier(channel_binding_mismatch(), 1);
        let provider = std::sync::Arc::new(provider);
        let mut handles = Vec::new();
        for _ in 0..8 {
            let provider = provider.clone();
            handles.push(tokio::spawn(async move {
                provider.fleet.get_or_verify_index_client(0).await
            }));
        }
        for handle in handles {
            assert!(
                handle.await.unwrap().is_err(),
                "Bootstrap state fails closed"
            );
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Other verification failures may be transient: they are still retried,
    /// and they do not start a backoff.
    #[tokio::test(start_paused = true)]
    async fn other_verification_failures_are_retried_without_backoff() {
        use std::sync::atomic::Ordering;
        let (provider, calls) = provider_with_failing_verifier(
            crate::BackendVerifyError::Other("Attestation request timed out".to_string()),
            4,
        );
        let attempts = Fleet::INLINE_VERIFY_RETRIES + 1;
        assert!(provider.fleet.get_or_verify_index_client(0).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), attempts);
        assert!(provider.fleet.get_or_verify_index_client(0).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2 * attempts);
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
            ) -> Result<reqwest::Client, crate::BackendVerifyError> {
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
            ) -> Result<reqwest::Client, crate::BackendVerifyError> {
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
            placement: Default::default(),
            request_priority: 0,
            model: "test-model".to_string(),
            messages: vec![ChatMessage {
                reasoning_content: None,
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
            service_tier: None,
            modalities: None,
            original_request: None,
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
            ) -> Result<reqwest::Client, crate::BackendVerifyError> {
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
            ) -> Result<reqwest::Client, crate::BackendVerifyError> {
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

    fn verifier_rotation_provider(count: usize) -> Provider {
        use crate::InferenceProvider;
        use std::sync::Arc;

        struct NoopVerifier;

        #[async_trait::async_trait]
        impl crate::BackendVerifier for NoopVerifier {
            async fn create_verified_client(
                &self,
                _base_url: &str,
            ) -> Result<reqwest::Client, crate::BackendVerifyError> {
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
        provider.set_backend_count(count);
        provider
    }

    fn user_msg(content: &str) -> crate::ChatMessage {
        role_msg(crate::MessageRole::User, content)
    }

    fn role_msg(role: crate::MessageRole, content: &str) -> crate::ChatMessage {
        crate::ChatMessage {
            reasoning_content: None,
            role,
            content: Some(serde_json::Value::String(content.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    #[test]
    fn fleet_candidate_indices_none_preserves_unrestricted_order() {
        // Given: an unwarmed four-backend fleet.
        let provider = rotation_provider(4);

        // When / Then: `allowed=None` preserves the exact pre-affinity order.
        assert_eq!(
            provider.fleet.candidate_indices(7, 4, None),
            vec![3, 0, 2, 1]
        );
    }

    #[test]
    fn fleet_candidate_indices_restricts_preferred_and_spill_to_group() {
        // Given.
        let provider = rotation_provider(4);
        let group = [0, 2];

        // When.
        let candidates = provider.fleet.candidate_indices(3, 4, Some(&group));

        // Then: preferred is group[route_key % group.len()] and every spill
        // candidate remains in the same key group.
        assert_eq!(candidates[0], group[3 % group.len()]);
        assert!(candidates.iter().all(|index| group.contains(index)));
    }

    #[test]
    fn fleet_candidate_indices_restricted_group_uses_full_u64_route_key() {
        // Given: a route key whose high 32 bits affect the group modulo.
        let provider = rotation_provider(4);
        let group = [0, 1, 2];
        let route_key = u64::from(u32::MAX) + 2;

        // When: the candidate order is restricted to the pinned key group.
        let candidates = provider.fleet.candidate_indices(route_key, 4, Some(&group));

        // Then: the full-width route key selects the preferred group member.
        assert_eq!(
            candidates[0],
            group[(route_key % group.len() as u64) as usize]
        );
    }

    #[test]
    fn fleet_candidate_indices_single_member_group_ignores_route_key() {
        // Given.
        let provider = rotation_provider(4);
        let group = [2];

        // When / Then.
        for route_key in [0, 1, 7, u64::MAX] {
            assert_eq!(
                provider.fleet.candidate_indices(route_key, 4, Some(&group)),
                vec![2]
            );
        }
    }

    #[test]
    fn fleet_pinned_conversation_stays_in_its_key_group() {
        // Given: established assistant history and a two-backend key group.
        let provider = rotation_provider(4);
        provider
            .fleet
            .set_backend_keys(HashMap::from([("key-a".to_string(), vec![0, 2])]));
        let messages = vec![
            user_msg("synthetic initial turn"),
            role_msg(crate::MessageRole::Assistant, "synthetic answer"),
            user_msg("synthetic follow-up"),
        ];

        // When.
        let indices: Vec<_> = (0..8)
            .map(|_| {
                provider
                    .fleet
                    .acquire_index(&messages, Some("KEY-A"))
                    .expect("rotation active")
                    .index()
            })
            .collect();

        // Then.
        assert!(indices.iter().all(|index| [0, 2].contains(index)));
        assert!(indices.iter().all(|index| *index == indices[0]));
    }

    #[test]
    fn fleet_backend_count_change_clears_key_restriction() {
        // Given: an empty-message request pinned to a single nonzero index.
        let provider = rotation_provider(4);
        provider
            .fleet
            .set_backend_keys(HashMap::from([("key-a".to_string(), vec![2])]));
        assert_eq!(
            provider
                .fleet
                .acquire_index(&[], Some("key-a"))
                .expect("rotation active")
                .index(),
            2
        );

        // When: the healthy count changes, invalidating index bindings.
        provider.set_backend_count(3);

        // Then: the stale map is cleared and routing fails open to today's
        // unrestricted empty-message index zero.
        assert_eq!(
            provider
                .fleet
                .acquire_index(&[], Some("key-a"))
                .expect("rotation active")
                .index(),
            0
        );
    }

    #[test]
    fn fleet_unknown_pinned_key_routes_unrestricted() {
        // Given.
        let provider = rotation_provider(4);
        provider
            .fleet
            .set_backend_keys(HashMap::from([("known-key".to_string(), vec![2])]));

        // When / Then: an unknown pin does not produce an empty candidate set.
        assert_eq!(
            provider
                .fleet
                .acquire_index(&[], Some("unknown-key"))
                .expect("rotation active")
                .index(),
            0
        );
    }

    #[test]
    fn fleet_unknown_key_warning_is_rate_limited() {
        // Given: a fresh Fleet and a fixed epoch-millisecond timestamp.
        let provider = rotation_provider(4);
        let now_ms = 1_000_000;

        // When / Then: the first occurrence warns, an immediate repeat is
        // suppressed, and the interval boundary permits the next warning.
        assert!(provider.fleet.should_warn_unknown_key(now_ms));
        assert!(!provider.fleet.should_warn_unknown_key(now_ms));
        assert!(provider.fleet.should_warn_unknown_key(now_ms + 60_000));
    }

    #[test]
    fn fleet_empty_backend_key_map_is_unrestricted_without_warning() {
        // Given: the normal homogeneous-fleet discovery state.
        let provider = rotation_provider(4);
        provider.fleet.set_backend_keys(HashMap::new());

        // When / Then: empty discovery state takes the silent path, not the
        // stale-client warning path.
        assert!(matches!(
            provider.fleet.key_group("key-a", 4),
            super::fleet::KeyGroup::Unrestricted
        ));
    }

    #[test]
    fn fleet_key_affinity_kill_switch_values_disable_restriction() {
        // Given / When / Then: the documented false values disable affinity;
        // other and missing values preserve the default-enabled behavior.
        assert!(!super::fleet::key_affinity_value_enabled(Some("0")));
        assert!(!super::fleet::key_affinity_value_enabled(Some("false")));
        assert!(!super::fleet::key_affinity_value_enabled(Some("FALSE")));
        assert!(super::fleet::key_affinity_value_enabled(Some("1")));
        assert!(super::fleet::key_affinity_value_enabled(None));
    }

    #[test]
    fn select_index_returns_none_when_rotation_disabled() {
        // localhost → no rotation parts → rotation_count()==0 → canonical
        // fallback path. Many tests (and cold-start) depend on this.
        let provider = create_test_provider();
        let msgs = vec![user_msg("hello")];
        assert_eq!(provider.fleet.select_index(&msgs, None), None);
    }

    #[test]
    fn select_index_is_prefix_hash_mod_count_with_no_stats() {
        // With no TTFT samples recorded, selection is pure prefix affinity:
        // `prefix_router.route(messages) % count`.
        let provider = rotation_provider(8);
        let msgs = vec![user_msg("a stable system prompt")];
        let expected = (provider.fleet.prefix_router.route(&msgs) % 8) as usize;
        let got = provider
            .fleet
            .select_index(&msgs, None)
            .expect("rotation active");
        assert_eq!(got, expected);
        // Deterministic / stable across calls (same prefix → same backend).
        assert_eq!(provider.fleet.select_index(&msgs, None), Some(expected));
    }

    #[test]
    fn select_index_is_stable_across_provider_histories() {
        let provider_a = rotation_provider(3);
        let provider_b = rotation_provider(3);

        for index in 0..100 {
            let message_a = vec![user_msg(&format!("provider-a-prefix-{index}"))];
            let message_b = vec![user_msg(&format!("provider-b-prefix-{}", 100 - index))];
            provider_a.fleet.select_index(&message_a, None);
            provider_b.fleet.select_index(&message_b, None);
        }

        let shared = vec![user_msg("shared prefix across Cloud API processes")];
        assert_eq!(
            provider_a.fleet.select_index(&shared, None),
            provider_b.fleet.select_index(&shared, None)
        );
    }

    #[test]
    fn acquire_index_keeps_a_small_burst_then_spills_deterministically() {
        let provider = rotation_provider(3);
        let messages = vec![user_msg("shared hot prefix")];
        let primary = provider
            .fleet
            .select_index(&messages, None)
            .expect("rotation active");

        let leases: Vec<_> = (0..9)
            .map(|_| {
                provider
                    .fleet
                    .acquire_index(&messages, None)
                    .expect("rotation active")
            })
            .collect();
        let indices: Vec<_> = leases.iter().map(|lease| lease.index()).collect();
        assert_eq!(&indices[..4], &[primary; 4]);
        assert!(indices[4..8].iter().all(|index| *index == indices[4]));
        assert_ne!(indices[4], primary);
        assert_ne!(indices[8], primary);
        assert_ne!(indices[8], indices[4]);
        assert_eq!(provider.fleet.active_prefix_loads(), 1);

        drop(leases);
        assert_eq!(provider.fleet.active_prefix_loads(), 0);
        let after_release = provider
            .fleet
            .acquire_index(&messages, None)
            .expect("rotation active");
        assert_eq!(after_release.index(), primary);
    }

    #[test]
    fn colliding_prefixes_have_key_dependent_spill_orders() {
        let provider = rotation_provider(3);
        let mut by_primary: std::collections::HashMap<usize, (u64, Vec<usize>)> =
            std::collections::HashMap::new();
        let mut collision = None;

        for index in 0..1_000 {
            let messages = vec![user_msg(&format!("collision-prefix-{index}"))];
            let route_key = provider.fleet.prefix_router.route(&messages);
            let candidates = provider.fleet.candidate_indices(route_key, 3, None);
            let primary = candidates[0];
            if let Some((other_key, other_candidates)) = by_primary.get(&primary) {
                if candidates[1..] != other_candidates[1..] {
                    collision = Some((*other_key, other_candidates.clone(), route_key, candidates));
                    break;
                }
            } else {
                by_primary.insert(primary, (route_key, candidates));
            }
        }

        let (first_key, first, second_key, second) =
            collision.expect("find same-primary keys with different spill orders");
        assert_eq!(first_key % 3, second_key % 3);
        assert_eq!(first[0], second[0]);
        assert_ne!(first[1..], second[1..]);
    }

    #[test]
    fn acquire_index_scopes_live_load_to_each_prefix() {
        let provider = rotation_provider(3);
        let first = vec![user_msg("first prefix")];
        let second = vec![user_msg("unrelated prefix")];
        let second_primary = provider
            .fleet
            .select_index(&second, None)
            .expect("rotation active");

        let held: Vec<_> = (0..8)
            .map(|_| {
                provider
                    .fleet
                    .acquire_index(&first, None)
                    .expect("rotation active")
            })
            .collect();
        let unrelated = provider
            .fleet
            .acquire_index(&second, None)
            .expect("rotation active");

        assert_eq!(unrelated.index(), second_primary);
        drop(unrelated);
        drop(held);
        assert_eq!(provider.fleet.active_prefix_loads(), 0);
    }

    #[test]
    fn acquire_index_is_deterministic_across_independent_fleets() {
        let provider_a = rotation_provider(3);
        let provider_b = rotation_provider(3);
        let messages = vec![user_msg("shared prefix across processes")];

        let leases_a: Vec<_> = (0..12)
            .map(|_| {
                provider_a
                    .fleet
                    .acquire_index(&messages, None)
                    .expect("rotation active")
            })
            .collect();
        let leases_b: Vec<_> = (0..12)
            .map(|_| {
                provider_b
                    .fleet
                    .acquire_index(&messages, None)
                    .expect("rotation active")
            })
            .collect();

        assert_eq!(
            leases_a
                .iter()
                .map(|lease| lease.index())
                .collect::<Vec<_>>(),
            leases_b
                .iter()
                .map(|lease| lease.index())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn follow_up_conversation_is_sticky_across_later_history_and_fleets() {
        let provider_a = rotation_provider(3);
        let provider_b = rotation_provider(3);
        let initial = [
            role_msg(crate::MessageRole::System, "shared system prefix"),
            user_msg("synthetic conversation one"),
        ];
        let turn_two = vec![
            initial[0].clone(),
            initial[1].clone(),
            role_msg(crate::MessageRole::Assistant, "synthetic answer one"),
            user_msg("synthetic follow-up two"),
        ];
        let turn_three = vec![
            turn_two[0].clone(),
            turn_two[1].clone(),
            turn_two[2].clone(),
            turn_two[3].clone(),
            role_msg(crate::MessageRole::Assistant, "synthetic answer two"),
            user_msg("synthetic follow-up three"),
        ];

        let turn_two_a = provider_a
            .fleet
            .acquire_index(&turn_two, None)
            .expect("rotation active");
        let turn_three_a = provider_a
            .fleet
            .acquire_index(&turn_three, None)
            .expect("rotation active");
        let turn_three_b = provider_b
            .fleet
            .acquire_index(&turn_three, None)
            .expect("rotation active");

        assert_eq!(turn_two_a.index(), turn_three_a.index());
        assert_eq!(turn_two_a.index(), turn_three_b.index());
    }

    #[test]
    fn follow_up_conversation_does_not_spill_while_requests_overlap() {
        let provider = rotation_provider(3);
        let messages = vec![
            role_msg(crate::MessageRole::System, "shared system prefix"),
            user_msg("synthetic initial turn"),
            role_msg(crate::MessageRole::Assistant, "synthetic answer"),
            user_msg("synthetic follow-up"),
        ];

        let leases: Vec<_> = (0..12)
            .map(|_| {
                provider
                    .fleet
                    .acquire_index(&messages, None)
                    .expect("rotation active")
            })
            .collect();

        assert!(leases
            .iter()
            .all(|lease| lease.index() == leases[0].index()));
    }

    #[test]
    fn distinct_conversations_with_one_prefix_can_reach_all_backends() {
        let provider = rotation_provider(3);
        let mut seen = [false; 3];

        for index in 0..1_000 {
            let messages = vec![
                role_msg(crate::MessageRole::System, "shared system prefix"),
                user_msg(&format!("synthetic conversation {index}")),
                role_msg(crate::MessageRole::Assistant, "synthetic answer"),
            ];
            let lease = provider
                .fleet
                .acquire_index(&messages, None)
                .expect("rotation active");
            seen[lease.index()] = true;
            if seen.iter().all(|value| *value) {
                break;
            }
        }

        assert!(seen.into_iter().all(|value| value));
    }

    #[test]
    fn acquire_index_respects_backend_counts_one_through_four() {
        let messages = vec![user_msg("backend count coverage")];
        for count in 1..=4 {
            let provider = rotation_provider(count);
            let leases: Vec<_> = (0..20)
                .map(|_| {
                    provider
                        .fleet
                        .acquire_index(&messages, None)
                        .expect("rotation active")
                })
                .collect();
            assert!(leases.iter().all(|lease| lease.index() < count));
            if count > 1 {
                assert!(
                    leases
                        .iter()
                        .map(|lease| lease.index())
                        .collect::<std::collections::HashSet<_>>()
                        .len()
                        > 1
                );
            }
        }
    }

    #[test]
    fn acquire_index_tolerates_backend_count_changes_with_live_leases() {
        let provider = rotation_provider(3);
        let messages = vec![user_msg("topology change prefix")];
        let held: Vec<_> = (0..9)
            .map(|_| {
                provider
                    .fleet
                    .acquire_index(&messages, None)
                    .expect("rotation active")
            })
            .collect();

        provider.set_backend_count(2);
        let after_shrink: Vec<_> = (0..8)
            .map(|_| {
                provider
                    .fleet
                    .acquire_index(&messages, None)
                    .expect("rotation active")
            })
            .collect();
        assert!(after_shrink.iter().all(|lease| lease.index() < 2));

        provider.set_backend_count(4);
        let after_growth: Vec<_> = (0..16)
            .map(|_| {
                provider
                    .fleet
                    .acquire_index(&messages, None)
                    .expect("rotation active")
            })
            .collect();
        assert!(after_growth.iter().all(|lease| lease.index() < 4));

        drop(after_growth);
        drop(after_shrink);
        drop(held);
        assert_eq!(provider.fleet.active_prefix_loads(), 0);
    }

    #[test]
    fn acquire_index_is_thread_safe_and_balances_a_sustained_hot_prefix() {
        use std::sync::{Arc, Barrier};

        let provider = rotation_provider(3);
        let fleet = provider.fleet.clone();
        let messages = Arc::new(vec![user_msg("concurrent hot prefix")]);
        let acquired = Arc::new(Barrier::new(13));
        let release = Arc::new(Barrier::new(13));
        let (sender, receiver) = std::sync::mpsc::channel();
        let workers: Vec<_> = (0..12)
            .map(|_| {
                let fleet = fleet.clone();
                let messages = messages.clone();
                let acquired = acquired.clone();
                let release = release.clone();
                let sender = sender.clone();
                std::thread::spawn(move || {
                    let lease = fleet
                        .acquire_index(&messages, None)
                        .expect("rotation active");
                    sender.send(lease.index()).expect("send selected index");
                    acquired.wait();
                    release.wait();
                    drop(lease);
                })
            })
            .collect();
        drop(sender);

        acquired.wait();
        let mut counts = [0usize; 3];
        for index in receiver.iter().take(12) {
            counts[index] += 1;
        }
        assert_eq!(counts, [4, 4, 4]);
        release.wait();
        for worker in workers {
            worker.join().expect("routing worker should not panic");
        }
        assert_eq!(fleet.active_prefix_loads(), 0);
    }

    #[test]
    fn acquire_index_keeps_empty_messages_on_zero() {
        let provider = rotation_provider(4);
        let leases: Vec<_> = (0..20)
            .map(|_| {
                provider
                    .fleet
                    .acquire_index(&[], None)
                    .expect("rotation active")
            })
            .collect();
        assert!(leases.iter().all(|lease| lease.index() == 0));
    }

    #[test]
    fn select_index_steers_off_pathologically_slow_preferred_backend() {
        let provider = rotation_provider(4);
        let msgs = vec![user_msg("route me")];
        let preferred = (provider.fleet.prefix_router.route(&msgs) % 4) as usize;

        // Warm the preferred backend as pathologically slow (>floor and >2× the
        // fastest), and a different backend as fast. Pick the fast index to be
        // distinct from `preferred`.
        let fast = (preferred + 1) % 4;
        for _ in 0..super::fleet::TTFT_WARMUP_SAMPLES {
            provider.fleet.record_ttft(preferred, 2000.0);
            provider.fleet.record_ttft(fast, 100.0);
        }

        let got = provider
            .fleet
            .select_index(&msgs, None)
            .expect("rotation active");
        assert_eq!(
            got, fast,
            "should steer from the slow preferred backend to the fastest warmed one"
        );

        // Below the slow ratio (only ~1.5× the fast peer) → keep affinity.
        let provider2 = rotation_provider(4);
        let preferred2 = (provider2.fleet.prefix_router.route(&msgs) % 4) as usize;
        let other2 = (preferred2 + 1) % 4;
        for _ in 0..super::fleet::TTFT_WARMUP_SAMPLES {
            provider2.fleet.record_ttft(preferred2, 900.0);
            provider2.fleet.record_ttft(other2, 600.0);
        }
        assert_eq!(
            provider2.fleet.select_index(&msgs, None),
            Some(preferred2),
            "below the 2x slow ratio, prefix affinity wins"
        );

        // Load spillover must not reintroduce the pathological backend.
        let leases: Vec<_> = (0..32)
            .map(|_| {
                provider
                    .fleet
                    .acquire_index(&msgs, None)
                    .expect("rotation active")
            })
            .collect();
        assert!(leases.iter().all(|lease| lease.index() != preferred));
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
            ) -> Result<reqwest::Client, crate::BackendVerifyError> {
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
    fn set_backend_keys_same_map_preserves_index_state() {
        // Given: a verifier-backed fleet with a published key map and live index state.
        let provider = verifier_rotation_provider(4);
        let map = HashMap::from([("key-a".to_string(), vec![0, 2])]);
        provider.fleet.set_backend_keys(map.clone());
        *provider.fleet.index_clients[0].lock().unwrap() = Some(reqwest::Client::new());
        provider.fleet.record_ttft(0, 123.0);

        // When: discovery publishes equal map content again.
        provider.fleet.set_backend_keys(map);

        // Then: neither the pinned client nor its per-index measurements are cleared.
        assert!(provider.fleet.index_clients[0].lock().unwrap().is_some());
        let stats = provider.fleet.backend_stats.lock().unwrap();
        assert_eq!(stats[0].samples, 1);
        assert_eq!(stats[0].ttft_ewma_ms, 123.0);
    }

    #[test]
    fn set_backend_keys_changed_map_clears_verifier_clients_and_resets_stats() {
        // Given: a verifier-backed fleet with index state tied to the current key map.
        let provider = verifier_rotation_provider(4);
        provider
            .fleet
            .set_backend_keys(HashMap::from([("key-a".to_string(), vec![0, 2])]));
        *provider.fleet.index_clients[0].lock().unwrap() = Some(reqwest::Client::new());
        provider.fleet.record_ttft(0, 123.0);

        // When: the key map changes while backend count stays fixed.
        provider
            .fleet
            .set_backend_keys(HashMap::from([("key-b".to_string(), vec![1, 3])]));

        // Then: stale pinned clients and all index-bound measurements are cleared.
        assert!(provider.fleet.index_clients[0].lock().unwrap().is_none());
        let stats = provider.fleet.backend_stats.lock().unwrap();
        assert!(stats
            .iter()
            .all(|stat| stat.samples == 0 && stat.ttft_ewma_ms == 0.0));
    }

    #[test]
    fn set_backend_keys_empty_over_empty_preserves_index_stats() {
        // Given: a homogeneous fleet with the default empty key map and live stats.
        let provider = rotation_provider(4);
        provider.fleet.record_ttft(0, 123.0);

        // When: discovery publishes the same empty map.
        provider.fleet.set_backend_keys(HashMap::new());

        // Then: the common homogeneous update is a no-op.
        let stats = provider.fleet.backend_stats.lock().unwrap();
        assert_eq!(stats[0].samples, 1);
        assert_eq!(stats[0].ttft_ewma_ms, 123.0);
    }

    #[test]
    fn fallback_indices_for_restricts_pinned_key_group() {
        // Given: only indices 0 and 2 hold the pinned key.
        let provider = rotation_provider(4);
        provider
            .fleet
            .set_backend_keys(HashMap::from([("key-a".to_string(), vec![0, 2])]));

        // When.
        let order = provider.fleet.fallback_indices_for(0, Some("key-a"));

        // Then: the tried backend and both foreign-key backends are excluded.
        assert_eq!(order, vec![2]);
        assert!(!order.contains(&1));
        assert!(!order.contains(&3));
    }

    #[test]
    fn fallback_indices_for_unpinned_request_is_unrestricted() {
        // Given: a mixed-key fleet, but no request pin.
        let provider = rotation_provider(4);
        provider
            .fleet
            .set_backend_keys(HashMap::from([("key-a".to_string(), vec![0, 2])]));

        // When.
        let order = provider.fleet.fallback_indices_for(0, None);

        // Then: today's all-other-indices behavior is unchanged.
        assert_eq!(order, vec![1, 2, 3]);
    }

    #[test]
    fn fallback_indices_for_exhausted_pinned_group_is_empty() {
        // Given: the tried backend is the pinned key group's only member.
        let provider = rotation_provider(4);
        provider
            .fleet
            .set_backend_keys(HashMap::from([("key-a".to_string(), vec![1])]));

        // When.
        let order = provider.fleet.fallback_indices_for(1, Some("key-a"));

        // Then: no foreign-key backend is attempted.
        assert!(order.is_empty());
    }

    #[test]
    fn fallback_indices_for_unknown_key_is_unrestricted() {
        // Given: discovery has a key map that does not contain the request pin.
        let provider = rotation_provider(4);
        provider
            .fleet
            .set_backend_keys(HashMap::from([("known-key".to_string(), vec![2])]));

        // When.
        let order = provider.fleet.fallback_indices_for(0, Some("unknown-key"));

        // Then: UnknownKey keeps the existing fall-open policy.
        assert_eq!(order, vec![1, 2, 3]);
    }

    #[test]
    fn fallback_indices_orders_warmed_fastest_first_and_skips_tried() {
        let provider = rotation_provider(4);
        // Warm indices 1 (slow) and 3 (fast); leave 0 and 2 unwarmed.
        for _ in 0..super::fleet::TTFT_WARMUP_SAMPLES {
            provider.fleet.record_ttft(1, 800.0);
            provider.fleet.record_ttft(3, 120.0);
        }
        let order = provider.fleet.fallback_indices_for(0, None);
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
        let probe = TtftProbe::new(inner, stats.clone(), index, start, None);
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

    #[tokio::test]
    async fn ttft_probe_releases_route_lease_at_end_of_stream() {
        use tokio_stream::StreamExt;

        let provider = rotation_provider(3);
        let messages = vec![user_msg("streamed prefix")];
        let lease = provider
            .fleet
            .acquire_index(&messages, None)
            .expect("rotation active");
        assert_eq!(provider.fleet.active_prefix_loads(), 1);
        let inner: StreamingResult = Box::pin(futures_util::stream::empty());
        let probe = TtftProbe::new(
            inner,
            provider.fleet.backend_stats.clone(),
            lease.index(),
            std::time::Instant::now(),
            Some(lease),
        );
        tokio::pin!(probe);

        assert!(probe.next().await.is_none());
        assert_eq!(provider.fleet.active_prefix_loads(), 0);
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

    /// The general and fallback clients return a backend redirect as an error
    /// instead of following it.
    #[tokio::test]
    async fn general_and_fallback_clients_do_not_follow_redirects() {
        use crate::InferenceProvider;
        let server = MockServer::start().await;
        let moved = format!("{}/moved", server.uri());
        for (verb, route) in [
            ("POST", "/v1/chat/completions"),
            ("GET", "/v1/attestation/report"),
        ] {
            Mock::given(method(verb))
                .and(path(route))
                .respond_with(ResponseTemplate::new(307).insert_header("location", moved.as_str()))
                .mount(&server)
                .await;
        }
        Mock::given(path("/moved"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let provider = Provider::new(Config {
            base_url: server.uri(),
            api_key: None,
            completion_timeout_seconds: 5,
            control_timeout_seconds: 5,
        });

        // No rotation for an IP-literal URL: chat goes through the fallback client.
        let params: ChatCompletionParams = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .unwrap();
        let chat = provider.chat_completion(params, "hash".to_string()).await;
        assert!(
            matches!(
                chat,
                Err(CompletionError::HttpError {
                    status_code: 307,
                    ..
                })
            ),
            "{chat:?}"
        );

        // The attestation report is fetched with the general client.
        let report = provider
            .get_attestation_report("test-model".to_string(), None, None, None, false)
            .await;
        assert!(
            matches!(&report, Err(AttestationError::FetchError(msg)) if msg.contains("307")),
            "{report:?}"
        );
        server.verify().await;
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

    /// Smart placement hook in front of `Fleet::acquire_index`. Every test
    /// uses a single eligible host (or a keyed request) so `place()` is
    /// deterministic regardless of the rng.
    mod placement_hook {
        use super::{control_event, data_event, role_msg, rotation_provider, user_msg, Provider};
        use crate::attested::nearai::placement_report::PlacementRequest;
        use crate::placement_io::{
            PlacementHandles, PlacementIo, PlacementMetrics, RoutedAck, Write, METRIC_AFFINITY,
            METRIC_DECISIONS, METRIC_WRITES_DROPPED, WRITE_QUEUE_CAPACITY,
        };
        use crate::BackendHosts;
        use arc_swap::ArcSwap;
        use placement::affinity::{pin_id, AffinityKey, PinTable};
        use placement::decision::{AffinitySource, Placer};
        use placement::frame::{Lifecycle, Load, ReplicaState};
        use placement::policy::Tier;
        use placement::snapshot::{ReplicaView, RoutedCounts, Snapshot};
        use placement::SlotId;
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use tokio::sync::mpsc;

        const PIN_SECRET: [u8; 32] = [9u8; 32];

        #[derive(Default)]
        struct FakeMetrics {
            counts: Mutex<Vec<(String, i64, Vec<String>)>>,
            histograms: Mutex<Vec<(String, f64, Vec<String>)>>,
        }

        impl FakeMetrics {
            /// The tag sets of every sample recorded under histogram `name`.
            fn histogram_tags(&self, name: &str) -> Vec<Vec<String>> {
                self.histograms
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(n, _, _)| n == name)
                    .map(|(_, _, tags)| tags.clone())
                    .collect()
            }

            fn decisions_tagged(&self, tag: &str) -> i64 {
                self.counts
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(n, _, tags)| n == METRIC_DECISIONS && tags.iter().any(|t| t == tag))
                    .map(|(_, v, _)| *v)
                    .sum()
            }
        }

        impl PlacementMetrics for FakeMetrics {
            fn record_count(&self, name: &str, value: i64, tags: &[&str]) {
                self.counts.lock().unwrap().push((
                    name.to_string(),
                    value,
                    tags.iter().map(|t| t.to_string()).collect(),
                ));
            }
            fn record_histogram(&self, name: &str, value: f64, tags: &[&str]) {
                self.histograms.lock().unwrap().push((
                    name.to_string(),
                    value,
                    tags.iter().map(|t| t.to_string()).collect(),
                ));
            }
        }

        fn now_ms() -> u64 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        }

        /// A `built_ms` / sample time that stays fresh for about
        /// `MAX_FUTURE_SKEW_MS + FRESH_MAX_MS` (5 s) of test stall: it sits at
        /// the edge of the allowed future skew (further ahead is rejected as
        /// clock skew), and counts as age 0 until the wall clock passes it.
        fn fresh_ms() -> u64 {
            now_ms() + placement::consts::MAX_FUTURE_SKEW_MS
        }

        fn slot(host: &str, replica: u32) -> SlotId {
            SlotId {
                host: host.into(),
                replica,
            }
        }

        fn ready_state(index: u32, now: u64) -> ReplicaState {
            ReplicaState {
                index,
                engine_sampled_at_ms: Some(now),
                lifecycle_state: Lifecycle::Ready,
                engine_version: None,
                limits: Default::default(),
                load: Load {
                    running: Some(0),
                    queued: Some(0),
                    ..Load::default()
                },
                proxy_inflight: 0,
            }
        }

        /// A ready view of `host#replica`.
        fn ready_replica(host: &str, replica: u32, now: u64) -> ReplicaView {
            ReplicaView {
                slot: slot(host, replica),
                state: ready_state(replica, now),
            }
        }

        /// A ready view of `host#0`.
        fn ready_view(host: &str, now: u64) -> ReplicaView {
            ready_replica(host, 0, now)
        }

        /// A snapshot built at `built_ms` with one ready replica on `host`.
        fn snapshot(host: &str, built_ms: u64, pins: PinTable) -> Snapshot {
            Snapshot {
                built_ms,
                replicas: vec![ready_view(host, built_ms)],
                pins: Arc::new(pins),
                ..Snapshot::default()
            }
        }

        struct Harness {
            provider: Provider,
            metrics: Arc<FakeMetrics>,
            writes: mpsc::Receiver<Write>,
            io: Arc<PlacementIo>,
        }

        /// A 4-backend rotation provider with placement installed: `host_map`
        /// becomes the verified host map, `snap` the current snapshot.
        fn harness(host_map: &[(&str, usize)], snap: Snapshot) -> Harness {
            harness_with_count(host_map, 4, snap)
        }

        /// Like [`harness`], but the pushed host map claims `hosts_count`
        /// backends while the Fleet itself has 4.
        ///
        /// Placement only runs on a complete picture, so the map is filled
        /// to the Fleet's 4 backends with filler hosts, and every mapped host
        /// without a view in `snap` gets a draining (ineligible) one. The
        /// hosts `host_map` and `snap` name are the only eligible ones.
        fn harness_with_count(
            host_map: &[(&str, usize)],
            hosts_count: usize,
            snap: Snapshot,
        ) -> Harness {
            harness_on(rotation_provider(4), host_map, hosts_count, snap)
        }

        /// [`harness_with_count`] on a given 4-backend `provider`.
        fn harness_on(
            provider: Provider,
            host_map: &[(&str, usize)],
            hosts_count: usize,
            mut snap: Snapshot,
        ) -> Harness {
            let mut full: Vec<(String, usize)> =
                host_map.iter().map(|(h, i)| (h.to_string(), *i)).collect();
            for index in 0..4 {
                if !full.iter().any(|(_, i)| *i == index) {
                    full.push((format!("h-fill-{index}"), index));
                }
            }
            for (host, _) in &full {
                if !snap.replicas.iter().any(|v| &v.slot.host == host) {
                    let mut view = ready_view(host, snap.built_ms);
                    view.state.lifecycle_state = Lifecycle::Draining;
                    snap.replicas.push(view);
                }
            }
            install(provider, &full, hosts_count, snap)
        }

        /// A 4-backend rotation provider with exactly `host_map` pushed and
        /// `snap` as the current snapshot (no filling).
        fn harness_exact(
            host_map: &[(String, usize)],
            hosts_count: usize,
            snap: Snapshot,
        ) -> Harness {
            install(rotation_provider(4), host_map, hosts_count, snap)
        }

        /// Installs placement on `provider` with exactly `host_map` pushed
        /// and `snap` as the current snapshot.
        fn install(
            provider: Provider,
            host_map: &[(String, usize)],
            hosts_count: usize,
            snap: Snapshot,
        ) -> Harness {
            let metrics = Arc::new(FakeMetrics::default());
            let (io, writes) = PlacementIo::for_test(metrics.clone());
            io.snapshot.store(Arc::new(snap));
            let hosts = Arc::new(ArcSwap::from_pointee(BackendHosts::default()));
            provider.fleet.set_placement(PlacementHandles {
                placer: Arc::new(Placer::new(PIN_SECRET, Tier::Base)),
                io: io.clone(),
                hosts: hosts.clone(),
            });
            // Every mapped host has published: it holds an attested
            // replica-report key.
            provider.fleet.set_backend_hosts(BackendHosts {
                index_by_host: host_map.iter().map(|(h, i)| (h.clone(), *i)).collect(),
                keys: placement::KeyRegistry {
                    by_host: host_map
                        .iter()
                        .map(|(h, _)| (h.clone(), vec![report_key()]))
                        .collect(),
                },
                count: hosts_count,
            });
            // Discovery's push lands in the map the Valkey reader also reads.
            assert_eq!(hosts.load().index_by_host.len(), host_map.len());
            Harness {
                provider,
                metrics,
                writes,
                io,
            }
        }

        /// An attested replica-report key, as discovery records for a host
        /// that publishes frames.
        fn report_key() -> placement::snapshot::HostKey {
            let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]).verifying_key();
            placement::snapshot::HostKey {
                key_id: placement::frame::key_id(&key),
                key,
            }
        }

        /// Drops every attested replica-report key from `h`'s host map,
        /// keeping the host bindings: no host behind it has ever published.
        fn unpublish(h: &Harness) {
            let hosts = h.provider.fleet.backend_hosts();
            h.provider.fleet.set_backend_hosts(BackendHosts {
                index_by_host: hosts.index_by_host.clone(),
                keys: Default::default(),
                count: hosts.count,
            });
        }

        fn request(model: &str) -> PlacementRequest {
            PlacementRequest {
                model: model.to_string(),
                model_tag: format!("model:{model}"),
                request_id: "req-1".to_string(),
                org_id: "org-1".to_string(),
                prompt_tokens: 0,
                context_tokens: None,
                heavy: false,
                prefill_heavy: false,
                affinity: None,
                affinity_source: AffinitySource::None,
                priority: 0,
                size: "size:unknown",
            }
        }

        /// Messages whose legacy index is not `avoid`, so a placed index can
        /// be told apart from the legacy one.
        fn messages_avoiding(avoid: usize) -> Vec<crate::ChatMessage> {
            let legacy = rotation_provider(4);
            (0..1_000)
                .map(|i| vec![user_msg(&format!("placement prefix {i}"))])
                .find(|m| legacy.fleet.select_index(m, None) != Some(avoid))
                .expect("some prefix avoids the index")
        }

        fn legacy_indices(
            messages: &[crate::ChatMessage],
            pinned: Option<&str>,
            keys: Option<HashMap<String, Vec<usize>>>,
        ) -> Vec<usize> {
            let provider = rotation_provider(4);
            if let Some(keys) = keys {
                provider.fleet.set_backend_keys(keys);
            }
            let leases: Vec<_> = (0..12)
                .map(|_| {
                    provider
                        .fleet
                        .acquire_index(messages, pinned)
                        .expect("rotation active")
                })
                .collect();
            leases.iter().map(|l| l.index()).collect()
        }

        fn placed_indices(
            provider: &Provider,
            messages: &[crate::ChatMessage],
            pinned: Option<&str>,
            req: &PlacementRequest,
        ) -> Vec<usize> {
            let leases: Vec<_> = (0..12)
                .map(|_| {
                    provider
                        .fleet
                        .acquire_index_placed(messages, pinned, req)
                        .expect("not refused")
                        .expect("rotation active")
                })
                .collect();
            leases.iter().map(|l| l.index()).collect()
        }

        #[test]
        fn placed_host_maps_to_its_index() {
            let h = harness(
                &[("h-a", 2)],
                snapshot("h-a", fresh_ms(), PinTable::default()),
            );
            let messages = messages_avoiding(2);
            let req = request("z-ai/glm-5.3-flash");
            assert_eq!(
                placed_indices(&h.provider, &messages, None, &req),
                vec![2; 12]
            );
            assert_eq!(h.metrics.decisions_tagged("outcome:place"), 12);
        }

        /// There is no model allow-list: any model whose hosts publish
        /// frames is placed.
        #[test]
        fn any_model_whose_hosts_publish_is_placed() {
            let h = harness(
                &[("h-a", 2)],
                snapshot("h-a", fresh_ms(), PinTable::default()),
            );
            let messages = messages_avoiding(2);
            let req = request("acme/any-new-model");
            assert_eq!(
                placed_indices(&h.provider, &messages, None, &req),
                vec![2; 12]
            );
            assert_eq!(h.metrics.decisions_tagged("outcome:place"), 12);
        }

        /// A model whose hosts publish nothing (no attested replica-report
        /// key, so the reader never reads for it) keeps its existing
        /// routing, with no placement writes.
        #[test]
        fn model_without_publishing_hosts_is_legacy() {
            let mut h = harness_exact(&[], 0, Snapshot::default());
            let messages = messages_avoiding(2);
            let req = request("acme/any-new-model");
            assert_eq!(
                placed_indices(&h.provider, &messages, None, &req),
                legacy_indices(&messages, None, None)
            );
            assert!(h.writes.try_recv().is_err(), "no placement writes");
            assert_eq!(h.metrics.decisions_tagged("outcome:place"), 0);
        }

        /// No host behind this endpoint has ever published: the legacy path
        /// is taken silently, with no decision, histogram or latency sample,
        /// even over a snapshot that would place.
        #[test]
        fn never_published_model_emits_no_placement_metrics() {
            let mut h = harness(
                &[("h-a", 2)],
                snapshot("h-a", fresh_ms(), PinTable::default()),
            );
            unpublish(&h);
            let messages = messages_avoiding(2);
            let req = request("acme/any-new-model");
            assert_eq!(
                placed_indices(&h.provider, &messages, None, &req),
                legacy_indices(&messages, None, None)
            );
            let lease = h
                .provider
                .fleet
                .acquire_index_placed(&messages, None, &req)
                .expect("not refused")
                .expect("rotation active");
            lease.record_ttft_ms(5.0);
            lease.record_duration_ms(9.0);
            assert!(h.writes.try_recv().is_err(), "no placement writes");
            assert!(h.metrics.counts.lock().unwrap().is_empty());
            assert!(h.metrics.histograms.lock().unwrap().is_empty());
        }

        /// Hosts that have published but whose state is unavailable (Valkey
        /// down: an empty snapshot) are a real outage: every request still
        /// counts as `legacy/no_state`, tagged with its model, and its
        /// latency is recorded as `legacy`.
        #[test]
        fn published_model_outage_still_counts_no_state() {
            let h = harness_exact(&[("h-a".to_string(), 2)], 4, Snapshot::default());
            let messages = messages_avoiding(2);
            let req = request("acme/any-new-model");
            assert_eq!(
                placed_indices(&h.provider, &messages, None, &req),
                legacy_indices(&messages, None, None)
            );
            assert_eq!(h.metrics.decisions_tagged("reason:no_state"), 12);
            assert_eq!(h.metrics.decisions_tagged("model:acme/any-new-model"), 12);
            let lease = h
                .provider
                .fleet
                .acquire_index_placed(&messages, None, &req)
                .expect("not refused")
                .expect("rotation active");
            lease.record_ttft_ms(5.0);
            assert_eq!(
                h.metrics
                    .histogram_tags(crate::placement_io::METRIC_TTFT_MS),
                vec![vec![
                    "strategy:legacy".to_string(),
                    "selection:legacy".to_string(),
                    "size:unknown".to_string(),
                    "model:acme/any-new-model".to_string(),
                ]]
            );
        }

        #[test]
        fn unconfigured_placement_uses_existing_path_unchanged() {
            let provider = rotation_provider(4);
            let messages = messages_avoiding(2);
            let req = request("z-ai/glm-5.3-flash");
            assert_eq!(
                placed_indices(&provider, &messages, None, &req),
                legacy_indices(&messages, None, None)
            );
        }

        #[test]
        fn unmapped_host_falls_back() {
            let mut h = harness(
                &[("h-a", 2)],
                snapshot("h-z", fresh_ms(), PinTable::default()),
            );
            let messages = messages_avoiding(2);
            let req = request("z-ai/glm-5.3-flash");
            assert_eq!(
                placed_indices(&h.provider, &messages, None, &req),
                legacy_indices(&messages, None, None)
            );
            assert_eq!(h.metrics.decisions_tagged("reason:host_unmapped"), 12);
            assert!(h.writes.try_recv().is_err(), "legacy writes nothing");
        }

        #[test]
        fn partial_host_map_goes_legacy() {
            // Only one of the Fleet's 4 backends publishes (partial proxy
            // rollout): placing would starve the other three, so legacy.
            let mut h = harness_exact(
                &[("h-a".to_string(), 2)],
                4,
                snapshot("h-a", fresh_ms(), PinTable::default()),
            );
            let messages = messages_avoiding(2);
            let req = request("z-ai/glm-5.3-flash");
            assert_eq!(
                placed_indices(&h.provider, &messages, None, &req),
                legacy_indices(&messages, None, None)
            );
            assert_eq!(h.metrics.decisions_tagged("reason:incomplete"), 12);
            assert!(h.writes.try_recv().is_err(), "legacy writes nothing");
        }

        #[test]
        fn mapped_host_without_frames_goes_legacy() {
            // Every backend is mapped, but h-d has no replica view (e.g. its
            // proxy restarted with a key not discovered yet): legacy.
            let map: Vec<(String, usize)> = ["h-a", "h-b", "h-c", "h-d"]
                .iter()
                .enumerate()
                .map(|(i, h)| (h.to_string(), i))
                .collect();
            let built = fresh_ms();
            let snap = Snapshot {
                built_ms: built,
                replicas: vec![
                    ready_view("h-a", built),
                    ready_view("h-b", built),
                    ready_view("h-c", built),
                ],
                ..Snapshot::default()
            };
            let mut h = harness_exact(&map, 4, snap);
            let messages = messages_avoiding(2);
            let req = request("z-ai/glm-5.3-flash");
            assert_eq!(
                placed_indices(&h.provider, &messages, None, &req),
                legacy_indices(&messages, None, None)
            );
            assert_eq!(h.metrics.decisions_tagged("reason:incomplete"), 12);
            assert!(h.writes.try_recv().is_err(), "legacy writes nothing");
        }

        #[test]
        fn host_map_for_another_backend_count_is_unmapped() {
            // The pushed map was built for 3 backends but the Fleet now has 4:
            // its host -> index binding is stale, so the host is unmapped.
            let mut h = harness_with_count(
                &[("h-a", 2)],
                3,
                snapshot("h-a", fresh_ms(), PinTable::default()),
            );
            let messages = messages_avoiding(2);
            let req = request("z-ai/glm-5.3-flash");
            assert_eq!(
                placed_indices(&h.provider, &messages, None, &req),
                legacy_indices(&messages, None, None)
            );
            assert_eq!(h.metrics.decisions_tagged("reason:host_unmapped"), 12);
            assert!(h.writes.try_recv().is_err(), "legacy writes nothing");
        }

        #[test]
        #[cfg_attr(debug_assertions, should_panic(expected = "already installed"))]
        fn second_install_of_the_same_handles_is_refused() {
            let metrics = Arc::new(FakeMetrics::default());
            let (io, _writes) = PlacementIo::for_test(metrics);
            let handles = PlacementHandles {
                placer: Arc::new(Placer::new(PIN_SECRET, Tier::Base)),
                io,
                hosts: Arc::new(ArcSwap::from_pointee(BackendHosts::default())),
            };
            let first = rotation_provider(4);
            let second = rotation_provider(4);
            first.fleet.set_placement(handles.clone());
            // Release builds ignore the second install and keep legacy routing.
            second.fleet.set_placement(handles);
            let messages = messages_avoiding(2);
            let req = request("z-ai/glm-5.3-flash");
            assert_eq!(
                placed_indices(&second, &messages, None, &req),
                legacy_indices(&messages, None, None)
            );
        }

        #[test]
        fn decision_metrics_use_static_tags() {
            let h = harness(
                &[("h-a", 2)],
                snapshot("h-a", fresh_ms(), PinTable::default()),
            );
            let messages = messages_avoiding(2);
            let req = request("z-ai/glm-5.3-flash");
            placed_indices(&h.provider, &messages, None, &req);
            let counts = h.metrics.counts.lock().unwrap();
            let decision = counts
                .iter()
                .find(|(n, _, _)| n == METRIC_DECISIONS)
                .expect("decision metric");
            assert_eq!(
                decision.2,
                vec![
                    "outcome:place".to_string(),
                    "tier:base".to_string(),
                    "class:short".to_string(),
                    "strategy:short_clean".to_string(),
                    "priority_band:normal".to_string(),
                    "selection:best_of_two".to_string(),
                    "model:z-ai/glm-5.3-flash".to_string(),
                ]
            );
            let backlog = counts
                .iter()
                .filter(|(n, _, _)| n == METRIC_AFFINITY)
                .count();
            assert_eq!(backlog, 12, "one affinity count per decision");
        }

        #[test]
        fn stale_snapshot_falls_back() {
            let stale = now_ms() - 60_000;
            let h = harness(&[("h-a", 2)], snapshot("h-a", stale, PinTable::default()));
            let messages = messages_avoiding(2);
            let req = request("z-ai/glm-5.3-flash");
            assert_eq!(
                placed_indices(&h.provider, &messages, None, &req),
                legacy_indices(&messages, None, None)
            );
            assert_eq!(h.metrics.decisions_tagged("reason:stale"), 12);
        }

        #[test]
        fn e2ee_pinned_key_group_respected() {
            let keys = HashMap::from([("key-a".to_string(), vec![0, 1])]);
            let mut h = harness(
                &[("h-a", 2)],
                snapshot("h-a", fresh_ms(), PinTable::default()),
            );
            h.provider.fleet.set_backend_keys(keys.clone());
            let messages = vec![
                user_msg("synthetic initial turn"),
                role_msg(crate::MessageRole::Assistant, "synthetic answer"),
                user_msg("synthetic follow-up"),
            ];
            let req = request("z-ai/glm-5.3-flash");
            let got = placed_indices(&h.provider, &messages, Some("key-a"), &req);
            assert!(got.iter().all(|i| [0, 1].contains(i)), "{got:?}");
            assert_eq!(got, legacy_indices(&messages, Some("key-a"), Some(keys)));
            assert_eq!(h.metrics.decisions_tagged("reason:key_group"), 12);
            assert!(h.writes.try_recv().is_err(), "legacy writes nothing");
        }

        #[test]
        fn placement_records_routed_write() {
            let mut h = harness(
                &[("h-a", 2)],
                snapshot("h-a", fresh_ms(), PinTable::default()),
            );
            // The routed tokens are the pool's prompt estimate, never a
            // placement-side re-estimate of the messages.
            let messages = vec![user_msg(&"x".repeat(40))];
            let mut req = request("z-ai/glm-5.3-flash");
            req.prompt_tokens = 10;
            let before_s = now_ms() / 1000;
            let lease = h
                .provider
                .fleet
                .acquire_index_placed(&messages, None, &req)
                .expect("not refused")
                .expect("rotation active");
            assert_eq!(lease.index(), 2);
            match h.writes.try_recv().expect("routed write queued") {
                Write::Routed {
                    slot: s, tok, sec, ..
                } => {
                    assert_eq!(s, slot("h-a", 0));
                    assert_eq!(tok, 10);
                    assert!(sec >= before_s && sec <= now_ms() / 1000);
                }
                Write::Pin { .. } => panic!("keyless request writes no pin"),
            }
            assert!(h.writes.try_recv().is_err());
            // This node's own routed count feeds the next decision.
            let mine = h.provider.fleet.placement_mine(now_ms() / 1000);
            assert_eq!(
                mine.get(&slot("h-a", 0)).map(|p| (p.req, p.tok)),
                Some((1, 10))
            );
        }

        /// Places one keyless 10-token request on h-a#0 (backend 2) and
        /// returns its queued routed write's acknowledgement handle.
        fn place_one(h: &mut Harness) -> RoutedAck {
            let mut req = request("z-ai/glm-5.3-flash");
            req.prompt_tokens = 10;
            let lease = h
                .provider
                .fleet
                .acquire_index_placed(&messages_avoiding(2), None, &req)
                .expect("not refused")
                .expect("rotation active");
            assert_eq!(lease.index(), 2);
            match h.writes.try_recv().expect("routed write queued") {
                Write::Routed { ack, .. } => ack,
                Write::Pin { .. } => panic!("keyless request writes no pin"),
            }
        }

        /// Replaces the current snapshot with the same replicas, as a later
        /// reader cycle whose routed read was issued at `routed_read_ms` and
        /// saw `routed` for h-a#0.
        fn reread(h: &Harness, routed_read_ms: u64, routed: Option<(u32, u64)>) {
            let old = h.io.snapshot.load_full();
            h.io.snapshot.store(Arc::new(Snapshot {
                built_ms: old.built_ms,
                replicas: old.replicas.clone(),
                routed: routed
                    .map(|(req, tok)| {
                        (
                            slot("h-a", 0),
                            RoutedCounts {
                                req,
                                tok,
                                since_ms: routed_read_ms.saturating_sub(1_000),
                            },
                        )
                    })
                    .into_iter()
                    .collect(),
                routed_read_ms,
                pins: old.pins.clone(),
                ..Snapshot::default()
            }));
        }

        fn mine_on_a0(h: &Harness) -> Option<(u32, u64)> {
            h.provider
                .fleet
                .placement_mine(now_ms() / 1000)
                .get(&slot("h-a", 0))
                .map(|p| (p.req, p.tok))
        }

        #[test]
        fn acked_placement_before_read_is_not_counted_twice() {
            let mut h = harness(
                &[("h-a", 2)],
                snapshot("h-a", fresh_ms(), PinTable::default()),
            );
            let ack = place_one(&mut h);
            let acked_ms = now_ms();
            ack.set(acked_ms);

            // A read issued after the acknowledgement already holds the
            // placement in `routed`: this node adds nothing on top.
            reread(&h, acked_ms + 1, Some((1, 10)));
            assert_eq!(mine_on_a0(&h), None);

            // A read issued at the acknowledgement instant may have missed
            // it, so it is still counted locally.
            reread(&h, acked_ms, Some((0, 0)));
            assert_eq!(mine_on_a0(&h), Some((1, 10)));
        }

        #[test]
        fn queued_placement_is_counted() {
            let mut h = harness(
                &[("h-a", 2)],
                snapshot("h-a", fresh_ms(), PinTable::default()),
            );
            // The write is still in the queue (never acknowledged), so even
            // a read issued after the placement cannot hold it.
            let ack = place_one(&mut h);
            assert_eq!(ack.acked_ms(), None);
            reread(&h, now_ms() + 1_000, None);
            assert_eq!(mine_on_a0(&h), Some((1, 10)));
        }

        #[test]
        fn dropped_write_is_still_counted_locally() {
            let h = harness(
                &[("h-a", 2)],
                snapshot("h-a", fresh_ms(), PinTable::default()),
            );
            // Fill the write queue so the placement's routed write is
            // dropped: it never reaches Valkey and is never acknowledged.
            let filler = || Write::Routed {
                slot: slot("h-z", 0),
                tok: 1,
                sec: 0,
                ack: RoutedAck::default(),
            };
            for _ in 0..WRITE_QUEUE_CAPACITY {
                h.io.record(filler());
            }
            let mut req = request("z-ai/glm-5.3-flash");
            req.prompt_tokens = 10;
            h.provider
                .fleet
                .acquire_index_placed(&messages_avoiding(2), None, &req)
                .expect("not refused")
                .expect("rotation active");
            let dropped: i64 = h
                .metrics
                .counts
                .lock()
                .unwrap()
                .iter()
                .filter(|(n, _, _)| n == METRIC_WRITES_DROPPED)
                .map(|(_, v, _)| *v)
                .sum();
            assert_eq!(dropped, 1);

            // Every later read misses it; this node keeps counting it until
            // it leaves the routed window.
            reread(&h, now_ms() + 1_000, None);
            assert_eq!(mine_on_a0(&h), Some((1, 10)));
        }

        #[tokio::test]
        async fn itl_recorded_only_with_two_chunks() {
            use crate::attested::nearai::fleet::mean_itl_ms;
            use crate::attested::nearai::TtftProbe;
            use crate::placement_io::{METRIC_DURATION_MS, METRIC_ITL_MS, METRIC_TTFT_MS};
            use tokio_stream::StreamExt;

            // (last - first token chunk) / (chunks - 1), nothing below 2.
            assert_eq!(mean_itl_ms(60.0, 4), Some(20.0));
            assert_eq!(mean_itl_ms(60.0, 2), Some(60.0));
            assert_eq!(mean_itl_ms(60.0, 1), None);
            assert_eq!(mean_itl_ms(60.0, 0), None);

            // A token chunk carries a choice; the trailing usage-only chunk
            // (`choices: []`) never counts, nor does its arrival time.
            let token = || {
                let mut event = data_event();
                if let Some(crate::StreamChunk::Chat(chunk)) = event.chunk.as_mut() {
                    chunk.choices = serde_json::from_value(serde_json::json!([
                        {"index": 0, "delta": {"content": "x"}}
                    ]))
                    .unwrap();
                }
                event
            };
            let usage_only = data_event;

            for chunks in 0..4usize {
                let h = harness(
                    &[("h-a", 2)],
                    snapshot("h-a", fresh_ms(), PinTable::default()),
                );
                let mut req = request("z-ai/glm-5.3-flash");
                req.size = "size:le8k";
                let lease = h
                    .provider
                    .fleet
                    .acquire_index_placed(&messages_avoiding(2), None, &req)
                    .expect("not refused")
                    .expect("rotation active");
                let mut items = vec![Ok(control_event(": keepalive\n"))];
                items.extend((0..chunks).map(|_| Ok(token())));
                items.push(Ok(usage_only()));
                let inner: crate::StreamingResult = Box::pin(futures_util::stream::iter(items));
                let start = std::time::Instant::now() - std::time::Duration::from_millis(5);
                let probe = TtftProbe::new(
                    inner,
                    h.provider.fleet.backend_stats.clone(),
                    lease.index(),
                    start,
                    Some(lease),
                );
                tokio::pin!(probe);
                while probe.next().await.is_some() {}

                let duration = h.metrics.histogram_tags(METRIC_DURATION_MS);
                assert_eq!(duration.len(), 1, "{chunks} chunks");
                assert_eq!(duration[0][2], "size:le8k");
                assert_eq!(h.metrics.histogram_tags(METRIC_TTFT_MS).len(), 1);
                let itl = h.metrics.histogram_tags(METRIC_ITL_MS);
                if chunks >= 2 {
                    assert_eq!(itl, duration, "{chunks} chunks: same tags");
                } else {
                    assert!(itl.is_empty(), "{chunks} chunks: no ITL");
                }
            }
        }

        #[test]
        fn placed_lease_carries_replica_index() {
            // h-a (backend 2) publishes replicas 0 and 3; only 3 is ready.
            let built = fresh_ms();
            let mut draining = ready_replica("h-a", 0, built);
            draining.state.lifecycle_state = Lifecycle::Draining;
            let snap = Snapshot {
                built_ms: built,
                replicas: vec![draining, ready_replica("h-a", 3, built)],
                ..Snapshot::default()
            };
            let mut h = harness(&[("h-a", 2)], snap);
            let req = request("z-ai/glm-5.3-flash");
            let lease = h
                .provider
                .fleet
                .acquire_index_placed(&messages_avoiding(2), None, &req)
                .expect("not refused")
                .expect("rotation active");
            assert_eq!(lease.index(), 2);
            assert_eq!(lease.replica(), Some(3));
            // The routed counter and this node's ledger are per replica.
            match h.writes.try_recv().expect("routed write queued") {
                Write::Routed { slot: s, .. } => assert_eq!(s, slot("h-a", 3)),
                Write::Pin { .. } => panic!("keyless request writes no pin"),
            }
            let mine = h.provider.fleet.placement_mine(now_ms() / 1000);
            assert_eq!(mine.keys().collect::<Vec<_>>(), vec![&slot("h-a", 3)]);
        }

        #[test]
        fn legacy_lease_has_no_replica() {
            let stale = now_ms() - 60_000;
            let h = harness(&[("h-a", 2)], snapshot("h-a", stale, PinTable::default()));
            let messages = messages_avoiding(2);
            let glm = request("z-ai/glm-5.3-flash");
            let other = request("acme/any-new-model");
            for req in [&glm, &other] {
                let lease = h
                    .provider
                    .fleet
                    .acquire_index_placed(&messages, None, req)
                    .expect("not refused")
                    .expect("rotation active");
                assert_eq!(lease.replica(), None);
            }
            // A plain legacy acquire never carries one either.
            let lease = h
                .provider
                .fleet
                .acquire_index(&messages, None)
                .expect("rotation active");
            assert_eq!(lease.replica(), None);
            assert_eq!(h.metrics.decisions_tagged("reason:stale"), 2);
        }

        /// A host whose every replica is past freshness while another host is
        /// fresh (typically a skewed clock) would silently starve: the host
        /// map counts as incomplete and the Fleet routes legacy.
        #[test]
        fn all_stale_host_makes_fleet_legacy() {
            let stale = now_ms() - 60_000;
            let with_h_b = |samples: [u64; 2]| {
                let mut snap = snapshot("h-a", fresh_ms(), PinTable::default());
                for (replica, at) in samples.into_iter().enumerate() {
                    snap.replicas.push(ready_replica("h-b", replica as u32, at));
                }
                snap
            };
            let messages = messages_avoiding(2);
            let req = request("z-ai/glm-5.3-flash");

            let mut h = harness(&[("h-a", 2), ("h-b", 3)], with_h_b([stale, stale]));
            assert_eq!(
                placed_indices(&h.provider, &messages, None, &req),
                legacy_indices(&messages, None, None)
            );
            assert_eq!(h.metrics.decisions_tagged("reason:host_stale"), 12);
            assert!(h.writes.try_recv().is_err(), "legacy writes nothing");

            // One fresh replica on h-b: the host is visible, placement runs.
            let h = harness(&[("h-a", 2), ("h-b", 3)], with_h_b([stale, fresh_ms()]));
            placed_indices(&h.provider, &messages, None, &req);
            assert_eq!(h.metrics.decisions_tagged("reason:host_stale"), 0);
            assert_eq!(h.metrics.decisions_tagged("outcome:place"), 12);
        }

        #[test]
        fn kill_switch_snapshot_falls_back_without_a_replica() {
            let mut snap = snapshot("h-a", fresh_ms(), PinTable::default());
            snap.disabled = true;
            let mut h = harness(&[("h-a", 2)], snap);
            let messages = messages_avoiding(2);
            let req = request("z-ai/glm-5.3-flash");
            assert_eq!(
                placed_indices(&h.provider, &messages, None, &req),
                legacy_indices(&messages, None, None)
            );
            assert_eq!(h.metrics.decisions_tagged("reason:disabled"), 12);
            assert!(h.writes.try_recv().is_err(), "legacy writes nothing");
        }

        #[test]
        fn pin_write_is_recorded() {
            let key = AffinityKey::from_bytes([5u8; 16]);
            let pid_hex = pin_id(Tier::Base, &key, &PIN_SECRET).to_hex();
            let id: [u8; 16] = hex::decode(&pid_hex).unwrap().try_into().unwrap();
            // An existing pin to a host that is no longer eligible: the placer
            // re-homes the session and rewrites the pin.
            let now = now_ms();
            let mut pins = PinTable::default();
            pins.insert(id, slot("h-gone", 0), now);
            let mut snap = snapshot("h-a", fresh_ms(), pins);
            snap.host_boots = HashMap::from([("h-a".to_string(), "boot-a".to_string())]);
            let mut h = harness(&[("h-a", 2)], snap);
            let messages = vec![user_msg("keyed request")];
            let mut req = request("z-ai/glm-5.3-flash");
            req.affinity = Some(key);
            req.affinity_source = AffinitySource::Client;
            let lease = h
                .provider
                .fleet
                .acquire_index_placed(&messages, None, &req)
                .expect("not refused")
                .expect("rotation active");
            assert_eq!(lease.index(), 2);
            assert!(matches!(
                h.writes.try_recv().expect("routed write"),
                Write::Routed { .. }
            ));
            match h.writes.try_recv().expect("pin write queued") {
                Write::Pin {
                    id_hex,
                    slot: s,
                    at_ms,
                    boot,
                } => {
                    assert_eq!(id_hex, pid_hex);
                    assert_eq!(s, slot("h-a", 0));
                    assert!(at_ms >= now);
                    // The pin carries its host's current boot.
                    assert_eq!(boot.as_deref(), Some("boot-a"));
                }
                Write::Routed { .. } => panic!("expected a pin write"),
            }
        }

        /// Cross-task contract: proxy key -> registry -> proxy-sealed host
        /// frame -> reader `apply` -> `Snapshot` -> `Placer` / `Fleet`.
        mod contract {
            use super::*;
            use base64::Engine as _;
            use ed25519_dalek::{Signer, SigningKey};
            use placement::decision::Decision;
            use placement::frame::{key_id, HostReport, SIGNING_DOMAIN};
            use placement::snapshot::{HostKey, KeyRegistry};
            use rand::rngs::StdRng;
            use rand::SeedableRng;

            /// The golden fixture's host and signing key.
            const GOLDEN_HOST: &str = "host-a";
            const GOLDEN_SEED: [u8; 32] = [7u8; 32];

            /// The fields of an attested `ReplicaReportKey` event, as the
            /// proxy emits it (hex public key, derived key id).
            struct ProxyKey {
                key_id: String,
                public_key_hex: String,
                host_id: String,
            }

            fn proxy_key(signing: &SigningKey, host: &str) -> ProxyKey {
                let vk = signing.verifying_key();
                ProxyKey {
                    key_id: key_id(&vk),
                    public_key_hex: hex::encode(vk.to_bytes()),
                    host_id: host.to_string(),
                }
            }

            /// Builds the registry the way the pool's `backend_hosts()`
            /// does (it lives in `services`, which this crate cannot depend
            /// on): hex-decode the attested public key, verify it is a valid
            /// ed25519 point, keep the key id.
            fn registry(keys: &[ProxyKey]) -> KeyRegistry {
                let mut by_host: HashMap<String, Vec<HostKey>> = HashMap::new();
                for k in keys {
                    let bytes: [u8; 32] =
                        hex::decode(&k.public_key_hex).unwrap().try_into().unwrap();
                    by_host.entry(k.host_id.clone()).or_default().push(HostKey {
                        key_id: k.key_id.clone(),
                        key: ed25519_dalek::VerifyingKey::from_bytes(&bytes).unwrap(),
                    });
                }
                KeyRegistry { by_host }
            }

            /// Envelope JSON sealed exactly as inference-proxy seals it.
            fn proxy_seal(report: &HostReport, signing: &SigningKey) -> String {
                let frame = serde_json::to_string(report).unwrap();
                let mut msg = SIGNING_DOMAIN.to_vec();
                msg.extend_from_slice(frame.as_bytes());
                serde_json::json!({
                    "frame": frame,
                    "sig": base64::engine::general_purpose::STANDARD
                        .encode(signing.sign(&msg).to_bytes()),
                    "key_id": report.report_key_id,
                })
                .to_string()
            }

            /// A one-replica host frame for `host`, sealed by `signing`.
            fn frame_for(host: &str, signing: &SigningKey, at_ms: u64, ready: bool) -> String {
                let mut state = ready_state(0, at_ms);
                if !ready {
                    state.lifecycle_state = Lifecycle::Draining;
                }
                let report = HostReport {
                    schema: 1,
                    host_id: host.into(),
                    boot_id: "boot-a".into(),
                    seq: 1,
                    reported_at_ms: at_ms,
                    engine: "sglang".into(),
                    report_key_id: key_id(&signing.verifying_key()),
                    replicas: vec![state],
                };
                proxy_seal(&report, signing)
            }

            #[test]
            fn golden_frame_through_reader_places_its_host() {
                let signing = SigningKey::from_bytes(&GOLDEN_SEED);
                let reg = registry(&[proxy_key(&signing, GOLDEN_HOST)]);
                let golden =
                    include_str!("../../../../placement/tests/fixtures/host_frame_v1.json");
                let frames = HashMap::from([(GOLDEN_HOST.to_string(), golden.to_string())]);
                // The fixture's own clock: reported at 1700000000500.
                let now = 1_700_000_000_500;
                let snap = crate::placement_io::snapshot_from_valkey_values(
                    &reg,
                    &frames,
                    &HashMap::new(),
                    now,
                );
                let slots: Vec<SlotId> = snap.replicas.iter().map(|v| v.slot.clone()).collect();
                assert_eq!(
                    slots,
                    vec![slot(GOLDEN_HOST, 0), slot(GOLDEN_HOST, 1)],
                    "golden frame accepted, one view per replica"
                );
                let input = placement::decision::PlaceInput {
                    model: "z-ai/glm-5.3-flash".into(),
                    prompt_tokens: 100,
                    context_tokens: None,
                    heavy: false,
                    prefill_heavy: false,
                    priority: 0,
                    affinity: None,
                    affinity_source: AffinitySource::None,
                    now_ms: now,
                };
                let mut rng = StdRng::seed_from_u64(1);
                match Placer::new(PIN_SECRET, Tier::Base).place(
                    &input,
                    &snap,
                    &HashMap::new(),
                    &mut rng,
                ) {
                    Decision::Place { slot, .. } => assert_eq!(slot.host, GOLDEN_HOST),
                    Decision::Legacy { reason, .. } => panic!("legacy: {}", reason.as_str()),
                    Decision::Refused { .. } => panic!("a short request is never refused"),
                }
            }

            /// Four backends: h-a (0) and h-b (1) ready, h-c and h-d
            /// draining, all publishing fresh proxy-sealed frames; `routed`
            /// is what the other nodes' routed hashes hold per slot.
            fn fleet_index(routed: HashMap<SlotId, (u64, u64)>) -> usize {
                let signing = SigningKey::from_bytes(&GOLDEN_SEED);
                let hosts = [("h-a", true), ("h-b", true), ("h-c", false), ("h-d", false)];
                let keys: Vec<ProxyKey> =
                    hosts.iter().map(|(h, _)| proxy_key(&signing, h)).collect();
                let reg = registry(&keys);
                let at = fresh_ms();
                let frames: HashMap<String, String> = hosts
                    .iter()
                    .map(|(h, ready)| (h.to_string(), frame_for(h, &signing, at, *ready)))
                    .collect();
                let snap =
                    crate::placement_io::snapshot_from_valkey_values(&reg, &frames, &routed, at);
                assert_eq!(snap.replicas.len(), 4, "every proxy frame accepted");
                let map: Vec<(String, usize)> = hosts
                    .iter()
                    .enumerate()
                    .map(|(i, (h, _))| (h.to_string(), i))
                    .collect();
                let h = harness_exact(&map, 4, snap);
                let req = request("z-ai/glm-5.3-flash");
                let lease = h
                    .provider
                    .fleet
                    .acquire_index_placed(&messages_avoiding(0), None, &req)
                    .expect("not refused")
                    .expect("rotation active");
                assert_eq!(h.metrics.decisions_tagged("outcome:place"), 1);
                assert_eq!(lease.replica(), Some(0));
                lease.index()
            }

            #[test]
            fn proxy_frames_through_reader_place_on_the_fleet() {
                let index = fleet_index(HashMap::new());
                assert!([0, 1].contains(&index), "placed on a ready host: {index}");
            }

            #[test]
            fn routed_counts_through_reader_shift_the_fleet_decision() {
                // Other nodes routed heavy load to h-a in the current window:
                // the fleet must place on h-b, and vice versa.
                let heavy = (20, 200_000);
                let to_a = HashMap::from([(slot("h-a", 0), heavy)]);
                let to_b = HashMap::from([(slot("h-b", 0), heavy)]);
                for _ in 0..8 {
                    assert_eq!(fleet_index(to_a.clone()), 1);
                    assert_eq!(fleet_index(to_b.clone()), 0);
                }
            }
        }

        /// The replica hint, end to end through a mock upstream reached over
        /// the rotation URLs (`glm-i<N>.mock.test`, resolved to the mock).
        mod replica_hint {
            use super::*;
            use crate::attested::nearai::upstream_headers::{REPLICA_HINT, REPLICA_HINT_HOST};
            use crate::attested::nearai::Config;
            use crate::{ChatCompletionParams, InferenceProvider};
            use futures_util::TryStreamExt;
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};

            /// Hands out clients that resolve every rotation host to the
            /// mock upstream.
            struct ResolvingVerifier(std::net::SocketAddr);

            #[async_trait::async_trait]
            impl crate::BackendVerifier for ResolvingVerifier {
                async fn create_verified_client(
                    &self,
                    _base_url: &str,
                ) -> Result<reqwest::Client, crate::BackendVerifyError> {
                    let mut builder = reqwest::Client::builder();
                    for index in 0..4 {
                        builder = builder.resolve(&format!("glm-i{index}.mock.test"), self.0);
                    }
                    Ok(builder.build().unwrap())
                }
            }

            /// A 4-backend rotation provider whose index clients all reach
            /// `upstream`.
            fn upstream_provider(upstream: &MockServer) -> Provider {
                let addr = *upstream.address();
                let provider = Provider::new_with_verifier(
                    Config {
                        base_url: format!("http://glm.mock.test:{}", addr.port()),
                        api_key: None,
                        completion_timeout_seconds: 5,
                        control_timeout_seconds: 5,
                    },
                    Arc::new(std::sync::RwLock::new(
                        crate::spki_verifier::FingerprintState::Bootstrap,
                    )),
                    Arc::new(ResolvingVerifier(addr)),
                );
                provider.set_backend_count(4);
                provider
            }

            /// Answers JSON or SSE by the request's `stream` flag; a request
            /// to a host starting with `fail_host` gets a 503.
            fn respond(fail_host: Option<&'static str>) -> impl wiremock::Respond {
                move |request: &wiremock::Request| {
                    let host = request.headers["host"].to_str().unwrap_or_default();
                    if fail_host.is_some_and(|h| host.starts_with(h)) {
                        return ResponseTemplate::new(503);
                    }
                    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                    if body["stream"] == true {
                        let chunk = serde_json::json!({"id": "synthetic",
                            "object": "chat.completion.chunk", "created": 0,
                            "model": "synthetic-model", "choices": [{"index": 0,
                            "delta": {"content": "ok"}, "finish_reason": "stop"}]});
                        ResponseTemplate::new(200).set_body_raw(
                            format!("data: {chunk}\n\ndata: [DONE]\n\n"),
                            "text/event-stream",
                        )
                    } else {
                        ResponseTemplate::new(200).set_body_json(serde_json::json!({
                            "id": "synthetic", "object": "chat.completion", "created": 0,
                            "model": "synthetic-model", "choices": [{"index": 0,
                            "message": {"role": "assistant", "content": "ok"},
                            "finish_reason": "stop"}],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1,
                                "total_tokens": 2}
                        }))
                    }
                }
            }

            async fn mock_upstream(fail_host: Option<&'static str>) -> MockServer {
                let upstream = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/chat/completions"))
                    .respond_with(respond(fail_host))
                    .mount(&upstream)
                    .await;
                upstream
            }

            fn params(model: &str) -> ChatCompletionParams {
                serde_json::from_value(serde_json::json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "synthetic replica hint"}],
                }))
                .unwrap()
            }

            /// Sends `params` once non-streaming and once streaming.
            async fn send_both(provider: &Provider, params: ChatCompletionParams) {
                provider
                    .chat_completion(params.clone(), "synthetic-hash-json".into())
                    .await
                    .expect("json completion");
                let stream = provider
                    .chat_completion_stream(params, "synthetic-hash-sse".into())
                    .await
                    .expect("sse completion");
                let _: Vec<_> = stream.try_collect().await.expect("sse stream");
            }

            fn header_values(requests: &[wiremock::Request], name: &str) -> Vec<Option<String>> {
                requests
                    .iter()
                    .map(|r| r.headers.get(name).map(|v| v.to_str().unwrap().to_string()))
                    .collect()
            }

            fn hints(requests: &[wiremock::Request]) -> Vec<Option<String>> {
                header_values(requests, REPLICA_HINT)
            }

            fn host_hints(requests: &[wiremock::Request]) -> Vec<Option<String>> {
                header_values(requests, REPLICA_HINT_HOST)
            }

            /// h-a (backend 2) publishes replicas 0 (draining) and 3 (ready).
            fn placed_snapshot(built_ms: u64) -> Snapshot {
                let mut draining = ready_replica("h-a", 0, built_ms);
                draining.state.lifecycle_state = Lifecycle::Draining;
                Snapshot {
                    built_ms,
                    replicas: vec![draining, ready_replica("h-a", 3, built_ms)],
                    ..Snapshot::default()
                }
            }

            #[tokio::test]
            async fn placed_request_sends_replica_hint_header() {
                let upstream = mock_upstream(None).await;
                let h = harness_on(
                    upstream_provider(&upstream),
                    &[("h-a", 2)],
                    4,
                    placed_snapshot(fresh_ms()),
                );
                send_both(&h.provider, params("z-ai/glm-5.3-flash")).await;

                let requests = upstream.received_requests().await.unwrap();
                assert_eq!(hints(&requests), vec![Some("3".to_string()); 2]);
                for request in &requests {
                    let host = request.headers["host"].to_str().unwrap();
                    assert!(host.starts_with("glm-i2."), "sent to backend 2: {host}");
                }
                assert_eq!(h.metrics.decisions_tagged("outcome:place"), 2);
            }

            /// The replica hint travels with the host placement chose, so the
            /// proxy can refuse it when the rotation index has drifted to a
            /// different host since discovery.
            #[tokio::test]
            async fn placed_request_sends_replica_host_header() {
                let upstream = mock_upstream(None).await;
                let h = harness_on(
                    upstream_provider(&upstream),
                    &[("h-a", 2)],
                    4,
                    placed_snapshot(fresh_ms()),
                );
                send_both(&h.provider, params("z-ai/glm-5.3-flash")).await;

                let requests = upstream.received_requests().await.unwrap();
                assert_eq!(hints(&requests), vec![Some("3".to_string()); 2]);
                assert_eq!(host_hints(&requests), vec![Some("h-a".to_string()); 2]);
            }

            /// A client-supplied host hint never reaches an upstream: not as
            /// a header on a legacy or canonical request, not over the
            /// placed host, and not in the body.
            #[tokio::test]
            async fn client_cannot_inject_replica_host() {
                let mut injected = params("z-ai/glm-5.3-flash");
                injected
                    .extra
                    .insert(REPLICA_HINT_HOST.to_string(), serde_json::json!("h-evil"));

                let placed_upstream = mock_upstream(None).await;
                let placed = harness_on(
                    upstream_provider(&placed_upstream),
                    &[("h-a", 2)],
                    4,
                    placed_snapshot(fresh_ms()),
                );
                send_both(&placed.provider, injected.clone()).await;
                let placed_requests = placed_upstream.received_requests().await.unwrap();
                assert_eq!(
                    host_hints(&placed_requests),
                    vec![Some("h-a".to_string()); 2]
                );

                let upstream = mock_upstream(None).await;
                let stale = harness_on(
                    upstream_provider(&upstream),
                    &[("h-a", 2)],
                    4,
                    placed_snapshot(now_ms() - 60_000),
                );
                let canonical = mock_upstream(None).await;
                let canonical_provider = Provider::new(Config::new(canonical.uri(), None, Some(5)));
                send_both(&stale.provider, injected.clone()).await;
                send_both(&canonical_provider, injected).await;

                let mut requests = upstream.received_requests().await.unwrap();
                requests.extend(canonical.received_requests().await.unwrap());
                assert_eq!(requests.len(), 4);
                assert_eq!(host_hints(&requests), vec![None; 4]);
                requests.extend(placed_requests);
                for request in &requests {
                    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                    assert!(
                        body.get(REPLICA_HINT_HOST).is_none(),
                        "client host hint reached the upstream body"
                    );
                }
            }

            /// The stream path records TTFT and duration once per streamed
            /// request, by strategy and selection (`legacy` when unplaced,
            /// nothing on a Fleet whose hosts publish no frames).
            #[tokio::test]
            async fn stream_latency_is_recorded_by_strategy() {
                use crate::placement_io::{METRIC_DURATION_MS, METRIC_TTFT_MS};
                let stream_once = |provider: Provider, model: &'static str| async move {
                    let stream = provider
                        .chat_completion_stream(params(model), "synthetic-hash-sse".into())
                        .await
                        .expect("sse completion");
                    let _: Vec<_> = stream.try_collect().await.expect("sse stream");
                };

                let upstream = mock_upstream(None).await;
                let h = harness_on(
                    upstream_provider(&upstream),
                    &[("h-a", 2)],
                    4,
                    placed_snapshot(fresh_ms()),
                );
                stream_once(h.provider, "z-ai/glm-5.3-flash").await;
                for name in [METRIC_TTFT_MS, METRIC_DURATION_MS] {
                    let samples = h.metrics.histogram_tags(name);
                    assert_eq!(samples.len(), 1, "{name}");
                    assert_eq!(samples[0][0], "strategy:short_clean", "{name}");
                    assert!(samples[0][1].starts_with("selection:"), "{name}");
                    assert_ne!(samples[0][1], "selection:unknown", "{name}");
                }

                let upstream = mock_upstream(None).await;
                let stale = now_ms() - 60_000;
                let h = harness_on(
                    upstream_provider(&upstream),
                    &[("h-a", 2)],
                    4,
                    placed_snapshot(stale),
                );
                stream_once(h.provider, "z-ai/glm-5.3-flash").await;
                for name in [METRIC_TTFT_MS, METRIC_DURATION_MS] {
                    assert_eq!(
                        h.metrics.histogram_tags(name),
                        vec![vec![
                            "strategy:legacy".to_string(),
                            "selection:legacy".to_string(),
                            "size:unknown".to_string(),
                            "model:z-ai/glm-5.3-flash".to_string(),
                        ]],
                        "{name}"
                    );
                }

                let upstream = mock_upstream(None).await;
                let h = install(upstream_provider(&upstream), &[], 0, Snapshot::default());
                stream_once(h.provider, "acme/any-new-model").await;
                assert!(h.metrics.histogram_tags(METRIC_TTFT_MS).is_empty());
                assert!(h.metrics.histogram_tags(METRIC_DURATION_MS).is_empty());
            }

            /// Provider-side latency carries the pool's prompt-size bucket,
            /// on placed and legacy leases alike.
            #[tokio::test]
            async fn latency_metrics_tag_size_bucket() {
                use crate::placement_io::{METRIC_DURATION_MS, METRIC_TTFT_MS};
                let sized = |tokens: Option<u64>| {
                    let mut params = params("z-ai/glm-5.3-flash");
                    params.placement.prompt_tokens = tokens;
                    params
                };
                let stream_once = |provider: Provider, params: ChatCompletionParams| async move {
                    let stream = provider
                        .chat_completion_stream(params, "synthetic-hash-sse".into())
                        .await
                        .expect("sse completion");
                    let _: Vec<_> = stream.try_collect().await.expect("sse stream");
                };

                for (built_ms, tokens, strategy, size) in [
                    (
                        fresh_ms(),
                        Some(20_000),
                        "strategy:short_clean",
                        "size:le32k",
                    ),
                    (fresh_ms(), None, "strategy:short_clean", "size:unknown"),
                    (
                        now_ms() - 60_000,
                        Some(150_000),
                        "strategy:legacy",
                        "size:gt100k",
                    ),
                    (
                        now_ms() - 60_000,
                        Some(5_000),
                        "strategy:legacy",
                        "size:le8k",
                    ),
                ] {
                    let upstream = mock_upstream(None).await;
                    let h = harness_on(
                        upstream_provider(&upstream),
                        &[("h-a", 2)],
                        4,
                        placed_snapshot(built_ms),
                    );
                    stream_once(h.provider, sized(tokens)).await;
                    for name in [METRIC_TTFT_MS, METRIC_DURATION_MS] {
                        let samples = h.metrics.histogram_tags(name);
                        assert_eq!(samples.len(), 1, "{name} {tokens:?}");
                        assert_eq!(samples[0][0], strategy, "{name} {tokens:?}");
                        assert_eq!(samples[0][2], size, "{name} {tokens:?}");
                    }
                }
            }

            #[tokio::test]
            async fn legacy_request_sends_no_replica_hint() {
                // Rotation with a legacy lease: a stale snapshot, for any
                // model.
                let upstream = mock_upstream(None).await;
                let stale = now_ms() - 60_000;
                let h = harness_on(
                    upstream_provider(&upstream),
                    &[("h-a", 2)],
                    4,
                    placed_snapshot(stale),
                );
                send_both(&h.provider, params("z-ai/glm-5.3-flash")).await;
                send_both(&h.provider, params("acme/any-new-model")).await;
                assert_eq!(h.metrics.decisions_tagged("reason:stale"), 4);

                // No rotation at all: the canonical URL.
                let canonical = mock_upstream(None).await;
                let provider = Provider::new(Config::new(canonical.uri(), None, Some(5)));
                send_both(&provider, params("z-ai/glm-5.3-flash")).await;

                let mut requests = upstream.received_requests().await.unwrap();
                requests.extend(canonical.received_requests().await.unwrap());
                assert_eq!(hints(&requests), vec![None; 6]);
                assert_eq!(host_hints(&requests), vec![None; 6]);
            }

            #[tokio::test]
            async fn client_extra_cannot_inject_replica_hint() {
                let upstream = mock_upstream(None).await;
                let stale = now_ms() - 60_000;
                let h = harness_on(
                    upstream_provider(&upstream),
                    &[("h-a", 2)],
                    4,
                    placed_snapshot(stale),
                );
                let canonical = mock_upstream(None).await;
                let canonical_provider = Provider::new(Config::new(canonical.uri(), None, Some(5)));
                let mut injected = params("z-ai/glm-5.3-flash");
                injected
                    .extra
                    .insert(REPLICA_HINT.to_string(), serde_json::json!("3"));
                send_both(&h.provider, injected.clone()).await;
                send_both(&canonical_provider, injected).await;

                let mut requests = upstream.received_requests().await.unwrap();
                requests.extend(canonical.received_requests().await.unwrap());
                assert_eq!(requests.len(), 4);
                assert_eq!(hints(&requests), vec![None; 4]);
                for request in &requests {
                    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                    assert!(
                        body.get(REPLICA_HINT).is_none(),
                        "client replica hint reached the upstream body"
                    );
                }
            }

            /// Every replica h-a (backend 2) publishes is a full heavy-lane
            /// member: a heavy request fits none of them.
            fn saturated_snapshot(built_ms: u64) -> Snapshot {
                Snapshot {
                    built_ms,
                    replicas: (0..4)
                        .map(|r| {
                            let mut view = ready_replica("h-a", r, built_ms);
                            view.state.load.prefill_backlog_tokens =
                                Some(placement::consts::HEAVY_BACKLOG_CAP);
                            view
                        })
                        .collect(),
                    ..Snapshot::default()
                }
            }

            /// A request the pool classed heavy.
            fn heavy_params() -> ChatCompletionParams {
                let mut params = params("z-ai/glm-5.3-flash");
                params.placement = crate::PlacementContext {
                    prompt_tokens: Some(100_000),
                    context_tokens: Some(120_000),
                    heavy: true,
                    prefill_heavy: true,
                    ..Default::default()
                };
                params
            }

            /// Refusals are opt-in: while the refuse-on key is set, a refusal
            /// is a 429 (`CapacityRefused`) returned before any upstream
            /// request.
            #[tokio::test]
            async fn refuse_on_key_enables_429() {
                let upstream = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(respond(None))
                    .expect(0)
                    .mount(&upstream)
                    .await;
                let mut snap = saturated_snapshot(fresh_ms());
                snap.refuse_on = true;
                let h = harness_on(upstream_provider(&upstream), &[("h-a", 2)], 4, snap);

                let json = h
                    .provider
                    .chat_completion(heavy_params(), "synthetic-hash-json".into())
                    .await;
                assert!(matches!(json, Err(crate::CompletionError::CapacityRefused)));
                let sse = h
                    .provider
                    .chat_completion_stream(heavy_params(), "synthetic-hash-sse".into())
                    .await;
                assert!(matches!(sse, Err(crate::CompletionError::CapacityRefused)));
                assert_eq!(h.metrics.decisions_tagged("outcome:refused"), 2);
                assert_eq!(h.metrics.decisions_tagged("reason:refuse_off"), 0);
                assert!(upstream.received_requests().await.unwrap().is_empty());
            }

            /// Without the refuse-on key a refusal runs the legacy path
            /// (served, tagged `reason:refuse_off`, never counted as
            /// refused); every other decision stays live.
            #[tokio::test]
            async fn refusal_is_fallback_by_default() {
                let upstream = mock_upstream(None).await;
                let h = harness_on(
                    upstream_provider(&upstream),
                    &[("h-a", 2)],
                    4,
                    saturated_snapshot(fresh_ms()),
                );
                send_both(&h.provider, heavy_params()).await;

                assert_eq!(h.metrics.decisions_tagged("reason:refuse_off"), 2);
                assert_eq!(h.metrics.decisions_tagged("outcome:legacy"), 2);
                assert_eq!(h.metrics.decisions_tagged("outcome:refused"), 0);
                let refused = h
                    .metrics
                    .counts
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(n, _, _)| n == crate::placement_io::METRIC_REFUSED)
                    .count();
                assert_eq!(refused, 0);
                let requests = upstream.received_requests().await.unwrap();
                assert_eq!(requests.len(), 2, "every legacy request is served");
                assert_eq!(hints(&requests), vec![None; 2]);
            }

            /// The kill switch takes precedence over refuse-on: with both
            /// keys set, every request is Legacy(disabled) and served.
            #[tokio::test]
            async fn kill_switch_wins_over_refuse_on() {
                let upstream = mock_upstream(None).await;
                let mut snap = saturated_snapshot(fresh_ms());
                snap.disabled = true;
                snap.refuse_on = true;
                let h = harness_on(upstream_provider(&upstream), &[("h-a", 2)], 4, snap);
                send_both(&h.provider, heavy_params()).await;

                assert_eq!(h.metrics.decisions_tagged("reason:disabled"), 2);
                assert_eq!(h.metrics.decisions_tagged("outcome:refused"), 0);
                let requests = upstream.received_requests().await.unwrap();
                assert_eq!(requests.len(), 2, "every legacy request is served");
                assert_eq!(hints(&requests), vec![None; 2]);
            }

            /// A refusal passes the same fail-open gates as a placement: an
            /// incomplete host map, or one built for another backend count,
            /// demotes it to the legacy path, which serves the request.
            #[tokio::test]
            async fn refused_with_incomplete_host_map_is_fallback_not_429() {
                let upstream = mock_upstream(None).await;
                // Refusals on, so only the gate demotes them.
                let refusing = || {
                    let mut snap = saturated_snapshot(fresh_ms());
                    snap.refuse_on = true;
                    snap
                };
                // Only h-a is mapped, of 4 backends.
                let partial = install(
                    upstream_provider(&upstream),
                    &[("h-a".to_string(), 2)],
                    4,
                    refusing(),
                );
                send_both(&partial.provider, heavy_params()).await;
                assert_eq!(partial.metrics.decisions_tagged("reason:incomplete"), 2);
                assert_eq!(partial.metrics.decisions_tagged("outcome:refused"), 0);

                // A complete map pushed for 3 backends while the Fleet has 4.
                let stale_count =
                    harness_on(upstream_provider(&upstream), &[("h-a", 2)], 3, refusing());
                send_both(&stale_count.provider, heavy_params()).await;
                assert_eq!(stale_count.metrics.decisions_tagged("reason:incomplete"), 2);
                assert_eq!(stale_count.metrics.decisions_tagged("outcome:refused"), 0);

                let requests = upstream.received_requests().await.unwrap();
                assert_eq!(requests.len(), 4, "every legacy request is served");
                assert_eq!(hints(&requests), vec![None; 4]);
            }

            #[tokio::test]
            async fn fallback_index_sends_no_replica_hint() {
                // The placed backend answers 503: the retry on another index
                // lands on replicas the decision never saw, so it carries no
                // hint.
                let upstream = mock_upstream(Some("glm-i2.")).await;
                let h = harness_on(
                    upstream_provider(&upstream),
                    &[("h-a", 2)],
                    4,
                    placed_snapshot(fresh_ms()),
                );
                send_both(&h.provider, params("z-ai/glm-5.3-flash")).await;

                let requests = upstream.received_requests().await.unwrap();
                let by_host: Vec<(bool, Option<String>)> = requests
                    .iter()
                    .zip(hints(&requests))
                    .map(|(r, hint)| {
                        let host = r.headers["host"].to_str().unwrap();
                        (host.starts_with("glm-i2."), hint)
                    })
                    .collect();
                assert_eq!(
                    by_host,
                    vec![
                        (true, Some("3".to_string())),
                        (false, None),
                        (true, Some("3".to_string())),
                        (false, None),
                    ]
                );
            }

            /// The fast healthy-count poll: the count endpoint of
            /// `glm.mock.test` is `mock.test/backends/count`, resolved to a
            /// mock that also stands in as the provider's upstream.
            mod count_poll {
                use super::*;
                use crate::CountPoll;

                fn count_client(server: &MockServer) -> reqwest::Client {
                    reqwest::Client::builder()
                        .resolve("mock.test", *server.address())
                        .build()
                        .unwrap()
                }

                /// Answers `/backends/count` with `healthy`, or 503 for `None`.
                async fn count_server(healthy: Option<usize>) -> MockServer {
                    let server = MockServer::start().await;
                    let response = match healthy {
                        Some(n) => ResponseTemplate::new(200)
                            .set_body_json(serde_json::json!({ "healthy": n, "total": n })),
                        None => ResponseTemplate::new(503),
                    };
                    Mock::given(method("GET"))
                        .and(path("/backends/count"))
                        .respond_with(response)
                        .mount(&server)
                        .await;
                    server
                }

                /// A placing 4-backend Fleet (h-a on backend 2) over `server`.
                fn placing(server: &MockServer) -> Harness {
                    harness_on(
                        upstream_provider(server),
                        &[("h-a", 2)],
                        4,
                        snapshot("h-a", fresh_ms(), PinTable::default()),
                    )
                }

                fn acquire(h: &Harness) -> crate::attested::nearai::fleet::RouteLease {
                    h.provider
                        .fleet
                        .acquire_index_placed(
                            &messages_avoiding(2),
                            None,
                            &request("z-ai/glm-5.3-flash"),
                        )
                        .expect("not refused")
                        .expect("rotation active")
                }

                /// A count change is stored at once, so the host map built
                /// for the old count no longer places: no stale index is used
                /// while rediscovery runs.
                #[tokio::test]
                async fn count_change_makes_fleet_legacy_immediately() {
                    let server = count_server(Some(5)).await;
                    let h = placing(&server);
                    assert_eq!(acquire(&h).index(), 2, "places before the change");

                    let poll = h.provider.poll_backend_count(&count_client(&server)).await;
                    assert_eq!(poll, CountPoll::Changed { old: 4, new: 5 });
                    assert_eq!(h.provider.fleet.backend_count(), 5);
                    assert_eq!(acquire(&h).replica(), None, "legacy lease");
                    assert_eq!(h.metrics.decisions_tagged("outcome:place"), 1);
                    assert_eq!(h.metrics.decisions_tagged("outcome:legacy"), 1);
                }

                #[tokio::test]
                async fn count_read_failure_changes_nothing() {
                    let server = count_server(None).await;
                    let h = placing(&server);
                    let poll = h.provider.poll_backend_count(&count_client(&server)).await;
                    assert_eq!(poll, CountPoll::Failed);
                    assert_eq!(h.provider.fleet.backend_count(), 4);
                    assert_eq!(acquire(&h).index(), 2, "still places");

                    // The same count again is no change either.
                    let server = count_server(Some(4)).await;
                    let h = placing(&server);
                    let poll = h.provider.poll_backend_count(&count_client(&server)).await;
                    assert_eq!(poll, CountPoll::Unchanged);
                    assert_eq!(acquire(&h).index(), 2);
                }

                /// Only Fleets whose hosts have published are polled: no
                /// request at all for one without placement or without an
                /// attested replica-report key.
                #[tokio::test]
                async fn count_poll_skipped_for_unpublished_fleets() {
                    let server = count_server(Some(5)).await;
                    let client = count_client(&server);

                    let h = placing(&server);
                    unpublish(&h);
                    assert_eq!(
                        h.provider.poll_backend_count(&client).await,
                        CountPoll::Skipped
                    );
                    assert_eq!(h.provider.fleet.backend_count(), 4);

                    let bare = upstream_provider(&server);
                    assert_eq!(bare.poll_backend_count(&client).await, CountPoll::Skipped);

                    assert!(server.received_requests().await.unwrap().is_empty());
                }

                /// `h`'s current host map, re-pushed by a discovery cycle
                /// with `count`, `keys` attested hosts and `complete`.
                fn push(
                    h: &Harness,
                    generation: u64,
                    count: usize,
                    keyed: bool,
                    complete: bool,
                ) -> crate::DiscoveryPush {
                    let hosts = h.provider.fleet.backend_hosts();
                    crate::DiscoveryPush {
                        generation,
                        count,
                        keys: HashMap::new(),
                        hosts: BackendHosts {
                            index_by_host: hosts.index_by_host.clone(),
                            keys: if keyed {
                                hosts.keys.clone()
                            } else {
                                Default::default()
                            },
                            count,
                        },
                        complete,
                    }
                }

                /// A discovery cycle that began before a poll saw the new
                /// count pushes the old count and host map late: it is
                /// discarded, and the polled count stands. The poll keeps
                /// reporting the stale host map until a current push lands.
                #[tokio::test]
                async fn older_discovery_push_cannot_overwrite_newer_polled_count() {
                    let server = count_server(Some(5)).await;
                    let client = count_client(&server);
                    let h = placing(&server);
                    let started = h.provider.count_generation();

                    assert_eq!(
                        h.provider.poll_backend_count(&client).await,
                        CountPoll::Changed { old: 4, new: 5 }
                    );
                    assert!(!h.provider.apply_discovery(push(&h, started, 4, true, true)));
                    assert_eq!(h.provider.fleet.backend_count(), 5);
                    assert_eq!(h.provider.fleet.backend_hosts().count, 4);
                    assert_eq!(acquire(&h).replica(), None, "still legacy");

                    // The count holds but the host map is from the old one.
                    assert_eq!(
                        h.provider.poll_backend_count(&client).await,
                        CountPoll::HostMapStale
                    );

                    // A cycle started after the poll lands, and places again.
                    let current = h.provider.count_generation();
                    assert!(h.provider.apply_discovery(push(&h, current, 5, true, true)));
                    assert_eq!(h.provider.fleet.backend_hosts().count, 5);
                    assert_eq!(
                        h.provider.poll_backend_count(&client).await,
                        CountPoll::Unchanged
                    );
                    // A push older than that one is discarded too.
                    assert!(!h.provider.apply_discovery(push(&h, current, 5, true, true)));
                }

                /// A failed discovery cycle that saw no replica-report key
                /// keeps the last attested registry, so a publishing model
                /// keeps reporting its outage. A complete cycle that saw
                /// none clears it.
                #[tokio::test]
                async fn failed_discovery_keeps_last_registry() {
                    let server = count_server(Some(4)).await;
                    let h = placing(&server);
                    let generation = h.provider.count_generation();
                    assert!(h
                        .provider
                        .apply_discovery(push(&h, generation, 4, false, false)));
                    assert!(!h.provider.fleet.backend_hosts().keys.by_host.is_empty());
                    assert_eq!(acquire(&h).index(), 2, "still places");

                    let generation = h.provider.count_generation();
                    assert!(h
                        .provider
                        .apply_discovery(push(&h, generation, 4, false, true)));
                    assert!(h.provider.fleet.backend_hosts().keys.by_host.is_empty());
                    let before = h.metrics.counts.lock().unwrap().len();
                    assert_eq!(acquire(&h).replica(), None);
                    assert_eq!(h.metrics.counts.lock().unwrap().len(), before, "silent");
                }
            }

            /// A placed host id that is not a valid header value sends
            /// neither hint: the replica hint never goes out without its
            /// host.
            #[tokio::test]
            async fn invalid_host_header_value_suppresses_both_headers() {
                let upstream = mock_upstream(None).await;
                let bad = "bad\nhost";
                let h = harness_on(
                    upstream_provider(&upstream),
                    &[(bad, 2)],
                    4,
                    snapshot(bad, fresh_ms(), PinTable::default()),
                );
                send_both(&h.provider, params("z-ai/glm-5.3-flash")).await;

                let requests = upstream.received_requests().await.unwrap();
                assert_eq!(requests.len(), 2);
                assert_eq!(h.metrics.decisions_tagged("outcome:place"), 2);
                assert_eq!(hints(&requests), vec![None; 2]);
                assert_eq!(host_hints(&requests), vec![None; 2]);
            }
        }

        #[test]
        fn placement_request_reads_the_typed_context() {
            let key = AffinityKey::from_bytes([3u8; 16]);
            let mut params = placement_params(vec![user_msg("hi")]);
            params.model = "m".to_string();
            params.request_priority = -5;
            params.extra = HashMap::from([
                ("x_request_id".to_string(), "req-9".into()),
                ("x_org_id".to_string(), "org-9".into()),
            ]);
            params.placement = crate::PlacementContext {
                prompt_tokens: Some(120_000),
                context_tokens: Some(130_000),
                heavy: true,
                prefill_heavy: true,
                affinity: Some(key.clone()),
                affinity_source: AffinitySource::Prefix,
            };
            let req = PlacementRequest::from_params(&params);
            assert_eq!(req.model, "m");
            assert_eq!(req.priority, -5);
            assert_eq!(req.request_id, "req-9");
            assert_eq!(req.org_id, "org-9");
            assert_eq!(req.prompt_tokens, 120_000);
            assert_eq!(req.context_tokens, Some(130_000));
            assert!(req.heavy);
            assert!(req.prefill_heavy);
            let fingerprint = |k: &AffinityKey| pin_id(Tier::Base, k, &PIN_SECRET).to_hex();
            assert_eq!(
                req.affinity.as_ref().map(fingerprint),
                Some(fingerprint(&key))
            );
            assert_eq!(req.affinity_source, AffinitySource::Prefix);

            // No pool context: an unknown size, short, and no affinity. A
            // source without a key is dropped.
            params.placement = crate::PlacementContext {
                affinity_source: AffinitySource::Client,
                ..Default::default()
            };
            let req = PlacementRequest::from_params(&params);
            assert_eq!(
                (
                    req.prompt_tokens,
                    req.context_tokens,
                    req.heavy,
                    req.prefill_heavy
                ),
                (0, None, false, false)
            );
            assert!(req.affinity.is_none());
            assert_eq!(req.affinity_source, AffinitySource::None);
        }

        /// Legacy affinity keys in a client's extra are never read: only the
        /// typed context carries routing inputs.
        #[test]
        fn legacy_affinity_extra_is_ignored_by_placement() {
            let mut params = placement_params(vec![user_msg("hi")]);
            params.extra = HashMap::from([
                ("x_placement_affinity".to_string(), "03".repeat(16).into()),
                ("x_placement_affinity_source".to_string(), "client".into()),
            ]);
            let req = PlacementRequest::from_params(&params);
            assert!(req.affinity.is_none());
            assert_eq!(req.affinity_source, AffinitySource::None);
        }

        fn placement_params(messages: Vec<crate::ChatMessage>) -> crate::ChatCompletionParams {
            let mut params: crate::ChatCompletionParams =
                serde_json::from_value(serde_json::json!({"model": "m", "messages": []})).unwrap();
            params.messages = messages;
            params
        }

        #[test]
        fn concurrent_try_place_does_not_herd() {
            use std::sync::Barrier;

            // Two equally scored, eligible hosts (same lifecycle, same empty
            // load): the placer has no deterministic reason to prefer one
            // over the other, so any imbalance would come only from a race
            // in the ledger read-then-reserve step this test targets.
            let built = fresh_ms();
            let snap = Snapshot {
                built_ms: built,
                replicas: vec![ready_view("h-a", built), ready_view("h-b", built)],
                ..Snapshot::default()
            };
            let h = harness(&[("h-a", 0), ("h-b", 1)], snap);
            let fleet = h.provider.fleet.clone();
            let messages = Arc::new(vec![user_msg("concurrent placement race")]);

            const WORKERS: usize = 16;
            let start = Arc::new(Barrier::new(WORKERS));
            let (sender, receiver) = std::sync::mpsc::channel();
            let workers: Vec<_> = (0..WORKERS)
                .map(|_| {
                    let fleet = fleet.clone();
                    let messages = messages.clone();
                    let start = start.clone();
                    let sender = sender.clone();
                    std::thread::spawn(move || {
                        // Keyless requests: same model, no affinity, so every
                        // thread reaches `try_place`'s ledger critical
                        // section the same way.
                        let req = request("z-ai/glm-5.3-flash");
                        start.wait();
                        let lease = fleet
                            .acquire_index_placed(&messages, None, &req)
                            .expect("not refused")
                            .expect("placement active");
                        sender.send(lease.index()).expect("send placed index");
                    })
                })
                .collect();
            drop(sender);
            for worker in workers {
                worker.join().expect("placement worker should not panic");
            }

            let mut counts = [0usize; 2];
            for index in receiver.iter() {
                counts[index] += 1;
            }
            // The ledger lock around `mine_in -> place -> index_for_host ->
            // ledger_add` is what stops every concurrent request from
            // reading the same "both hosts idle" view and piling onto one
            // host. We don't assert an exact split — the placer's tie-break
            // and OS thread scheduling aren't required to produce one — only
            // that both equally scored hosts actually received traffic,
            // which a herd (all N on one host) would fail.
            assert!(
                counts[0] > 0 && counts[1] > 0,
                "placements herded onto one host: {counts:?}"
            );
        }
    }
}

#[cfg(test)]
mod priority_tests;
