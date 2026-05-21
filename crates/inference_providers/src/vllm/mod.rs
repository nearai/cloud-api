mod prefix_router;

use crate::spki_verifier::{FingerprintState, SharedTlsRoots};
use crate::{
    models::StreamOptions, sse_parser::new_sse_parser, ImageEditError, ImageGenerationError,
    PrivacyClassifyError, RerankError, ScoreError, *,
};
use async_trait::async_trait;
use prefix_router::PrefixRouter;
use reqwest::{header::HeaderValue, Client};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// Encryption header keys used in params.extra for passing encryption information
mod encryption_headers {
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
/// Both are tunable per-deployment via env vars (see `VLlmConfig::new`).
#[derive(Debug, Clone)]
pub struct VLlmConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    /// Total per-request timeout for completion-style operations.
    pub completion_timeout_seconds: i64,
    /// Total per-request timeout for control-plane operations and streaming TTFB.
    pub control_timeout_seconds: i64,
}

impl VLlmConfig {
    /// Default completion timeout. Reasoning models can spend several minutes
    /// on a single non-streaming request; 600s is a comfortable ceiling that
    /// still surfaces genuinely stuck requests.
    pub const DEFAULT_COMPLETION_TIMEOUT_SECS: i64 = 600;
    /// Default control timeout. Metadata/TTFB ops should resolve quickly.
    pub const DEFAULT_CONTROL_TIMEOUT_SECS: i64 = 90;

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

/// vLLM provider implementation
///
/// Provides inference through vLLM's OpenAI-compatible API endpoints.
/// Supports both chat completions and text completions with streaming.
pub struct VLlmProvider {
    config: VLlmConfig,
    /// General-purpose client for non-completion requests (attestation, models, etc.)
    client: Client,
    /// Lazily-filled bucket clients indexed by prefix bucket ID. Each slot starts
    /// empty and is filled on first use via inline backend verification. Once filled,
    /// the client maintains a persistent H2 connection to a specific verified backend.
    bucket_clients: Vec<std::sync::Mutex<Option<Client>>>,
    /// Prefix router: message-level trie mapping conversation prefixes to bucket IDs.
    prefix_router: Arc<PrefixRouter>,
    /// Maps request_hash → bucket_id during streaming (before chat_id is known).
    pending_buckets: Arc<std::sync::Mutex<HashMap<String, usize>>>,
    /// Maps chat_id → bucket_id for signature fetching on the correct backend.
    signature_buckets: Arc<std::sync::Mutex<HashMap<String, usize>>>,
    /// TLS fingerprint verification state (Bootstrap → Pinned or Blocked).
    /// Shared across the main client and all bucket clients.
    fingerprint_state: Arc<std::sync::RwLock<FingerprintState>>,
    /// Creates verified clients for lazy bucket initialization.
    /// When a bucket needs a client, the verifier connects to a backend,
    /// verifies its attestation, pins the fingerprint, and returns the client.
    backend_verifier: Option<Arc<dyn crate::BackendVerifier>>,
    /// Fallback client used when inline bucket verification exhausts all retries.
    /// Has completion-timeout read settings so long-running inference requests
    /// don't hit the 90s control-plane idle timeout. Does not pin TLS to a
    /// specific backend — requests are served without prefix-cache routing but
    /// are not dropped, ensuring inline verification failures degrade gracefully.
    fallback_client: Client,
    /// Bounds concurrent inline verifications to prevent thundering-herd pressure
    /// on inference-proxy GPU evidence collection at startup (when all buckets are
    /// empty and many requests arrive simultaneously). Configurable via the
    /// `INLINE_VERIFY_CONCURRENCY` environment variable (default: 4).
    verification_semaphore: Arc<Semaphore>,
    /// Cached TLS roots for building per-attempt rotation clients. Reused for
    /// the rotation-SNI fallback path so the fingerprint pin set stays
    /// consistent with the bucket clients.
    tls_roots: SharedTlsRoots,
    /// Most recent healthy backend count reported by discovery. Used by the
    /// rotation-SNI retry path to bound the number of distinct backends to
    /// fan out to when the sticky bucket returns 5xx. Discovery writes via
    /// `set_backend_count`; the chat paths read with `Ordering::Relaxed`
    /// (best-effort — a stale read just means we try one too few or one too
    /// many indices, both safe because the proxy wraps `index % healthy`).
    last_backend_count: AtomicUsize,
    /// Pre-parsed rotation parts derived from `config.base_url` at
    /// construction time, so we don't reparse on every retry. `None` for
    /// URLs that don't fit the rotation scheme (one-label host, IP literal,
    /// etc.) — in that case the rotation fallback is a no-op and the
    /// canonical-SNI error propagates as before.
    rotation_parts: Option<crate::rotation::UrlParts>,
    /// Maps request_hash → rotation index when the streaming canonical attempt
    /// fell over and a rotation-SNI attempt served the response instead.
    /// `pin_chat_connection` promotes this to `signature_rotation` once the
    /// chat_id is known, so `get_signature` can reuse the same rotation SNI.
    pending_rotation: Arc<std::sync::Mutex<HashMap<String, u64>>>,
    /// Maps chat_id → rotation index for the signature fetch path. Populated
    /// either by the non-streaming chat_completion (chat_id known at send
    /// time) or by `pin_chat_connection` once the stream's first chunk
    /// yields a chat_id.
    signature_rotation: Arc<std::sync::Mutex<HashMap<String, u64>>>,
}

impl VLlmProvider {
    /// Create a new vLLM provider with the given configuration.
    /// Without a `BackendVerifier`, bucket clients are pre-created eagerly
    /// (legacy behavior for tests and non-TEE environments).
    pub fn new(config: VLlmConfig) -> Self {
        let fingerprint_state = Arc::new(std::sync::RwLock::new(FingerprintState::Bootstrap));
        Self::new_with_fingerprint_state(config, fingerprint_state)
    }

    /// Create a new vLLM provider sharing an existing fingerprint state.
    /// Without a `BackendVerifier`, bucket clients are pre-created eagerly.
    pub fn new_with_fingerprint_state(
        config: VLlmConfig,
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
    /// Bucket clients are created lazily: on first use, the verifier connects to
    /// a backend, verifies attestation, pins the fingerprint, and returns a client
    /// whose H2 connection is pinned to that verified backend.
    pub fn new_with_verifier(
        config: VLlmConfig,
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
        config: VLlmConfig,
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
        config: VLlmConfig,
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
        let num_buckets = prefix_router.num_buckets();

        // Bucket clients: lazily filled when a verifier is available (each bucket
        // gets a verified client on first use), or eagerly pre-created (legacy).
        let bucket_clients: Vec<std::sync::Mutex<Option<Client>>> = if backend_verifier.is_some() {
            (0..num_buckets)
                .map(|_| std::sync::Mutex::new(None))
                .collect()
        } else {
            (0..num_buckets)
                .map(|_| {
                    let builder = Client::builder()
                        .use_preconfigured_tls(tls_roots.build_config(fingerprint_state.clone()))
                        .pool_max_idle_per_host(1)
                        .http2_adaptive_window(true)
                        .connect_timeout(Duration::from_secs(5))
                        .read_timeout(completion_timeout);
                    // Bucket clients need the H2 connection to stay sticky to a
                    // single backend across long idle gaps; see
                    // `crate::bucket_keepalive`.
                    let c = crate::bucket_keepalive::apply(builder)
                        .build()
                        .expect("Failed to create bucket HTTP client");
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
            config,
            client,
            fallback_client,
            verification_semaphore,
            bucket_clients,
            prefix_router,
            pending_buckets: Arc::new(std::sync::Mutex::new(HashMap::new())),
            signature_buckets: Arc::new(std::sync::Mutex::new(HashMap::new())),
            fingerprint_state,
            backend_verifier,
            tls_roots,
            last_backend_count: AtomicUsize::new(0),
            rotation_parts,
            pending_rotation: Arc::new(std::sync::Mutex::new(HashMap::new())),
            signature_rotation: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Access the provider's configuration.
    pub fn config(&self) -> &VLlmConfig {
        &self.config
    }

    /// Get a reference to the shared fingerprint state.
    pub fn fingerprint_state(&self) -> Arc<std::sync::RwLock<FingerprintState>> {
        self.fingerprint_state.clone()
    }

    /// Add a verified SPKI fingerprint. Transitions Bootstrap → Pinned,
    /// or adds to existing Pinned set. Unblocks a Blocked provider.
    pub fn add_verified_fingerprint(&self, fingerprint: String) {
        self.fingerprint_state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .add_fingerprint(fingerprint);
    }

    /// Block all TLS connections (attestation verification failed).
    /// Only blocks from Bootstrap state — does not override existing Pinned fingerprints.
    pub fn block_connections(&self) {
        self.fingerprint_state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .block();
    }

    /// Returns the number of verified fingerprints currently pinned.
    pub fn pinned_fingerprint_count(&self) -> usize {
        self.fingerprint_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .pinned_count()
    }

    /// Spawn background tasks to pre-warm all bucket clients.
    ///
    /// Each empty bucket gets a background task that calls
    /// `get_or_verify_bucket_client`: it connects to a backend, verifies its
    /// attestation, and caches the resulting HTTP client. By the time user
    /// traffic arrives, most buckets are already filled and the inline
    /// verification cost has been paid upfront rather than on first use.
    ///
    /// Concurrency is bounded by `verification_semaphore` (the same limit
    /// that guards against thundering-herd pressure at startup), so the
    /// pre-warm tasks and any concurrent user requests share a single pool
    /// of attestation permits and don't amplify load on inference-proxy.
    ///
    /// No-op in three cases:
    /// - No `BackendVerifier` (legacy / non-TEE mode — buckets are eagerly
    ///   pre-filled at construction time).
    /// - Bootstrap state (`pinned_fingerprint_count() == 0`) — no verified
    ///   fingerprints yet, so every task would fail the security guard in
    ///   `get_or_verify_bucket_client` and log a spurious warn.
    /// - Blocked state (also `pinned_fingerprint_count() == 0`) — provider
    ///   has been explicitly blocked; attempting verification would only waste
    ///   attestation round-trips and fill logs with noise.
    pub fn pre_warm(self: Arc<Self>) {
        if self.backend_verifier.is_none() {
            return;
        }
        if self.pinned_fingerprint_count() == 0 {
            tracing::debug!(
                "Pre-warm skipped: no fingerprints pinned (Bootstrap or Blocked state)"
            );
            return;
        }
        let num_buckets = self.bucket_clients.len();
        tracing::info!(num_buckets = num_buckets, "Pre-warming bucket clients");
        for bucket_id in 0..num_buckets {
            let provider = self.clone();
            tokio::spawn(async move {
                match provider.get_or_verify_bucket_client(bucket_id).await {
                    Ok(_) => {
                        tracing::debug!(bucket = bucket_id, "Bucket pre-warm complete");
                    }
                    Err(e) => {
                        tracing::warn!(
                            bucket = bucket_id,
                            error = %e,
                            "Bucket pre-warm failed; will retry inline on first use"
                        );
                    }
                }
            });
        }
    }

    /// Maximum inline-verification retries when creating a verified bucket client.
    const INLINE_VERIFY_RETRIES: usize = 2;

    /// Get the client for a bucket, creating and verifying it inline if needed.
    /// On first use, connects to a backend via L4, fetches its attestation report,
    /// verifies it, pins the fingerprint, and caches the client.
    ///
    /// Concurrent inline verifications are bounded by `verification_semaphore`
    /// (Fix 1: prevents thundering-herd pressure on inference-proxy GPU evidence
    /// collection when all buckets are empty at startup).
    ///
    /// If all verification attempts fail, falls back to `fallback_client` so the
    /// request is served without prefix-cache routing rather than returning an
    /// error to the user (Fix 2: graceful degradation on attestation failure).
    async fn get_or_verify_bucket_client(
        &self,
        bucket_id: usize,
    ) -> Result<Client, CompletionError> {
        // Fast path: bucket already has a verified client.
        // reqwest::Client::clone is an Arc refcount bump — hold the lock briefly.
        {
            let guard = self.bucket_clients[bucket_id]
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
                // No verifier configured (legacy/test mode) — bucket should have
                // been pre-created eagerly; reaching here is a logic error.
                return Err(CompletionError::CompletionError(
                    "No backend verifier configured for lazy bucket creation".to_string(),
                ));
            }
        };

        // Acquire a semaphore permit before attempting attestation. This bounds
        // the number of concurrent inline verifications, preventing thundering-herd
        // pressure on inference-proxy GPU evidence collection at startup (when all
        // buckets are empty and many requests arrive simultaneously).
        //
        // The semaphore is never closed, so acquire() only returns Err on close —
        // treat that as a bug.
        //
        // Note on worst-case wait time: the permit is held for the entire retry
        // loop (INLINE_VERIFY_RETRIES + 1 attempts × control_timeout each). With
        // default values that is 3 × 90s = 270s per slot. Requests queueing behind
        // a saturated semaphore of size N can wait up to (queue_depth / N) × 270s.
        // In practice the first successful verification fills the bucket and all
        // subsequent waiters take the fast path (re-check after acquiring permit).
        let _permit = self
            .verification_semaphore
            .acquire()
            .await
            .expect("verification semaphore should never be closed");

        // Re-check after acquiring the permit: a concurrent request that held the
        // semaphore before us may have already filled this bucket.
        {
            let guard = self.bucket_clients[bucket_id]
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref client) = *guard {
                return Ok(client.clone());
            }
        }

        let mut last_err = None;
        for _attempt in 0..=Self::INLINE_VERIFY_RETRIES {
            match verifier.create_verified_client(&self.config.base_url).await {
                Ok(client) => {
                    // Double-check: another concurrent request may have filled
                    // this bucket while we were verifying. Use its client if so
                    // (avoids wasting the connection it established).
                    let mut guard = self.bucket_clients[bucket_id]
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(ref existing) = *guard {
                        return Ok(existing.clone());
                    }
                    *guard = Some(client.clone());
                    return Ok(client);
                }
                Err(e) => {
                    // Another request may have filled the bucket while we failed.
                    let guard = self.bucket_clients[bucket_id]
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(ref existing) = *guard {
                        return Ok(existing.clone());
                    }
                    drop(guard);

                    tracing::warn!(
                        bucket = bucket_id,
                        error = %e,
                        "Inline backend verification failed, retrying"
                    );
                    last_err = Some(e);
                }
            }
        }

        // All retry attempts exhausted.
        //
        // Only fall back to the general-purpose client when at least one
        // backend fingerprint has already been pinned (Pinned state). In that
        // case the fallback_client's TLS verifier will still reject any backend
        // whose SPKI fingerprint is unknown — so we degrade gracefully (no
        // prefix-cache routing) without bypassing attestation.
        //
        // In Bootstrap state (pinned_count == 0) no fingerprints have been
        // verified yet. fallback_client in Bootstrap mode would accept *any*
        // WebPKI-valid cert, silently bypassing SPKI pinning and TEE attestation
        // guarantees. Return Err instead so the pool can surface the failure.
        let err_msg = format!(
            "Inline backend verification failed after {} attempts: {}",
            Self::INLINE_VERIFY_RETRIES + 1,
            last_err.unwrap_or_default()
        );
        if self.pinned_fingerprint_count() > 0 {
            tracing::warn!(
                bucket = bucket_id,
                error = %err_msg,
                "Inline backend verification exhausted retries; serving with fallback client"
            );
            Ok(self.fallback_client.clone())
        } else {
            // Bootstrap: no fingerprints pinned yet. Fail safely.
            tracing::warn!(
                bucket = bucket_id,
                error = %err_msg,
                "Inline backend verification exhausted retries in Bootstrap state; \
                 refusing fallback to prevent unauthenticated connections"
            );
            Err(CompletionError::CompletionError(err_msg))
        }
    }

    /// Check if a CompletionError indicates a connection/transport failure
    /// (as opposed to an HTTP-level error from the backend).
    fn is_connection_error(err: &CompletionError) -> bool {
        match err {
            CompletionError::CompletionError(msg) => {
                // reqwest connection errors contain these keywords.
                // After send_streaming_request converts reqwest::Error to String,
                // this is the only way to detect transport failures.
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

    /// Clear a bucket's client so it will be re-verified on next use.
    /// Called on connection errors — prevents a stale client (whose H2
    /// connection dropped) from being reused with an unverified reconnection.
    fn clear_bucket(&self, bucket_id: usize) {
        *self.bucket_clients[bucket_id]
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Build HTTP request headers
    fn build_headers(&self) -> Result<reqwest::header::HeaderMap, String> {
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

    /// Hard cap on rotation fan-out. Mirrors `MAX_ROTATION_FANOUT` in the
    /// discovery path: a bogus `/backends/count` reading shouldn't let one
    /// 5xx burn an unbounded number of fresh-TCP handshakes.
    const MAX_ROTATION_FANOUT: usize = 256;

    /// Status codes that warrant a rotation-SNI retry. Mirrors the pool's
    /// `classify_retry_decision` ("retryable_http_5xx" + 429), but evaluated
    /// here so the rotation fallback fires *before* the canonical 5xx
    /// escapes to the pool's same-provider backoff loop (which would only
    /// re-hit the sticky bucket → same overloaded backend).
    fn is_rotation_retryable_status(status_code: u16) -> bool {
        status_code == 429 || (500..=599).contains(&status_code)
    }

    /// Healthy backend count clamped to the rotation fan-out cap. Returns 0
    /// when rotation is disabled (URL doesn't fit the rotation scheme, or
    /// discovery hasn't reported a count yet) — callers use that as the
    /// signal to skip the rotation fallback and propagate the original error.
    fn rotation_count(&self) -> usize {
        if self.rotation_parts.is_none() {
            return 0;
        }
        self.last_backend_count
            .load(Ordering::Relaxed)
            .min(Self::MAX_ROTATION_FANOUT)
    }

    /// Build the absolute URL `https://<canonical>-i<index>.<base><path>` for
    /// a rotation attempt at the given backend index. Returns `None` only if
    /// rotation parts are missing — callers should already have filtered via
    /// `rotation_count() > 0`.
    fn rotation_url(&self, index: u64, path: &str) -> Option<String> {
        let parts = self.rotation_parts.as_ref()?;
        let mut url = crate::rotation::rotation_base_url(parts, index)?;
        url.set_path(path);
        Some(url.to_string())
    }

    /// Build a one-shot reqwest client used for a single rotation-SNI
    /// attempt. We disable connection pooling (`pool_max_idle_per_host(0)`)
    /// so a follow-up attempt at index N+1 can't accidentally reuse the
    /// TLS/H2 connection that landed on index N — defeating the whole point
    /// of the rotation. Shares the per-provider fingerprint state so the
    /// same pinned SPKI set is enforced for every backend.
    fn build_rotation_client(&self) -> Result<Client, CompletionError> {
        Client::builder()
            .use_preconfigured_tls(self.tls_roots.build_config(self.fingerprint_state.clone()))
            .pool_max_idle_per_host(0)
            .http2_adaptive_window(true)
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(self.config.completion_timeout())
            .build()
            .map_err(|e| CompletionError::CompletionError(format!("rotation_client_build: {e}")))
    }

    /// Iterate every healthy backend by index until one returns a 2xx (or
    /// every backend has been exhausted). Called by `chat_completion` after
    /// the sticky bucket's canonical-SNI attempt returns 5xx/429: with H2
    /// pooling disabled on each per-index client, every attempt lands on a
    /// distinct backend, so a single overloaded SGLang can't poison the
    /// whole request.
    ///
    /// `canonical_err` is the error that triggered the fallback; if all
    /// rotation indices return retryable failures it surfaces as the final
    /// error to preserve the original `status_code` for `map_provider_error`.
    async fn try_chat_completion_rotation(
        &self,
        params: &ChatCompletionParams,
        headers: &reqwest::header::HeaderMap,
        timeout: Duration,
        canonical_err: CompletionError,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        let count = self.rotation_count();
        let mut last_error = canonical_err;
        for index in 0..count as u64 {
            let url = match self.rotation_url(index, "/v1/chat/completions") {
                Some(u) => u,
                None => continue,
            };
            let client = match self.build_rotation_client() {
                Ok(c) => c,
                Err(e) => {
                    last_error = e;
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
                    // Connect / network errors against this index — try the
                    // next backend; treat as retryable since the rotation
                    // listener pins to one backend by design (model-proxy
                    // PR #27).
                    last_error = if e.is_timeout() && !e.is_connect() {
                        CompletionError::Timeout {
                            operation: "chat_completion".to_string(),
                            timeout_seconds: timeout.as_secs(),
                        }
                    } else {
                        CompletionError::CompletionError(format_error_chain(&e))
                    };
                    tracing::debug!(
                        index, error = %last_error,
                        "Rotation-SNI chat_completion attempt errored, trying next backend"
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
                if Self::is_rotation_retryable_status(status_code) {
                    tracing::debug!(
                        index,
                        status_code,
                        "Rotation-SNI chat_completion backend still 5xx, trying next"
                    );
                    last_error = err;
                    continue;
                }
                // 4xx (other than 429) means the request itself is bad —
                // surface immediately rather than burn the rest of the
                // rotation set on the same client error.
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
                .insert(chat_id, index);
            tracing::info!(
                index,
                "Rotation-SNI chat_completion served by alternative backend"
            );
            return Ok(ChatCompletionResponseWithBytes {
                response: chat_completion_response,
                raw_bytes,
            });
        }
        Err(last_error)
    }

    /// Streaming sibling of `try_chat_completion_rotation`. Two failure modes
    /// land here:
    ///   - The canonical send returned HTTP 5xx/429 outright (e.g. nginx 502
    ///     when the inference-proxy container is restarting).
    ///   - The canonical send returned HTTP 200 but the first SSE chunk was
    ///     a `{"error":{"code":...}}` frame, which the parser now surfaces as
    ///     a typed `HttpError` (the SGLang queue-full path that inference-
    ///     proxy's SseTransformer forwards verbatim).
    ///
    /// We iterate every backend by rotation index until one returns a 200
    /// whose first SSE chunk is a real content event. Bytes already sent to
    /// the client are zero (peek happens before any chunk forwarding), so
    /// retrying the whole stream is safe.
    async fn try_chat_completion_stream_rotation(
        &self,
        params: &ChatCompletionParams,
        headers: &reqwest::header::HeaderMap,
        request_hash: &str,
        canonical_err: CompletionError,
    ) -> Result<StreamingResult, CompletionError> {
        let count = self.rotation_count();
        let mut last_error = canonical_err;
        for index in 0..count as u64 {
            let url = match self.rotation_url(index, "/v1/chat/completions") {
                Some(u) => u,
                None => continue,
            };
            let client = match self.build_rotation_client() {
                Ok(c) => c,
                Err(e) => {
                    last_error = e;
                    continue;
                }
            };
            let response = match self
                .send_streaming_request(&url, headers.clone(), params, Some(&client))
                .await
            {
                Ok(r) => r,
                Err(e) => match &e {
                    // 4xx other than 429 is a real client error (bad request,
                    // invalid params) — every backend would reject it the
                    // same way, so surface immediately rather than burn the
                    // remaining indices on a doomed request.
                    CompletionError::HttpError { status_code, .. }
                        if !Self::is_rotation_retryable_status(*status_code) =>
                    {
                        return Err(e);
                    }
                    // Everything else (retryable HttpError 5xx/429,
                    // `Timeout` from `send_streaming_request`'s TTFB guard,
                    // generic `CompletionError` for TLS/TCP/transport
                    // failures) is per-backend by construction: model-proxy
                    // PR #27 pins each `-iN` SNI to one backend, so the
                    // failure at index N tells us nothing about index N+1.
                    // Mirror the non-streaming sibling and try the next
                    // index instead of giving up.
                    _ => {
                        tracing::debug!(
                            index,
                            error = %e,
                            "Rotation-SNI stream attempt failed, trying next backend"
                        );
                        last_error = e;
                        continue;
                    }
                },
            };
            let parser = new_sse_parser(response.bytes_stream(), true);
            let stream: StreamingResult = Box::pin(parser);
            let mut peekable = StreamingResultExt::peekable(stream);
            let first_chunk_status =
                if let Some(Err(CompletionError::HttpError { status_code, .. })) =
                    peekable.peek().await
                {
                    if Self::is_rotation_retryable_status(*status_code) {
                        Some(*status_code)
                    } else {
                        None
                    }
                } else {
                    None
                };
            if let Some(status_code) = first_chunk_status {
                tracing::debug!(
                    index,
                    status_code,
                    "Rotation-SNI stream attempt: first chunk was an error, trying next backend"
                );
                last_error = CompletionError::HttpError {
                    status_code,
                    message: "Upstream stream emitted an error event".to_string(),
                    is_external: false,
                };
                drop(peekable);
                continue;
            }
            self.pending_rotation
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(request_hash.to_string(), index);
            tracing::info!(
                index,
                "Rotation-SNI chat_completion_stream served by alternative backend"
            );
            return Ok(Box::pin(peekable));
        }
        Err(last_error)
    }
}

#[async_trait]
impl InferenceProvider for VLlmProvider {
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

        // If this chat_id was served by a rotation-SNI fallback (sticky bucket
        // returned 5xx, so we walked backends by index until one took the
        // request), the signature lives on that *specific* backend — neither
        // the bucket-pinned client nor the general LB client can find it. We
        // build a one-shot rotation client targeting the same index and use
        // it FIRST; the existing bucket/general fallback runs only if that
        // attempt also misses.
        let rotation_index = self
            .signature_rotation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(chat_id)
            .copied();
        if let Some(index) = rotation_index {
            if let Some(rotation_url) = self.rotation_url(index, "") {
                let rotation_url =
                    format!("{}{}", rotation_url.trim_end_matches('/'), path_and_query);
                if let Ok(client) = self.build_rotation_client() {
                    match client
                        .get(&rotation_url)
                        .headers(headers.clone())
                        .timeout(timeout)
                        .send()
                        .await
                    {
                        Ok(response) if response.status().is_success() => {
                            return response.json().await.map_err(|e| {
                                CompletionError::CompletionError(format_error_chain(&e))
                            });
                        }
                        Ok(response) => {
                            tracing::debug!(
                                index,
                                status = response.status().as_u16(),
                                "Rotation-SNI signature fetch did not return 2xx, falling back to bucket/general"
                            );
                        }
                        Err(e) => {
                            tracing::debug!(
                                index,
                                error = %format_error_chain(&e),
                                "Rotation-SNI signature fetch errored, falling back to bucket/general"
                            );
                        }
                    }
                }
            }
        }

        // Use the bucket client for this chat_id to hit the same backend.
        // With HTTP/2 (ALPN-negotiated), all requests multiplex on one connection.
        // Under HTTP/1.1 fallback with concurrency, the bucket client may have
        // opened a second connection to a different backend — if we get 404,
        // retry once on the general-purpose client as a fallback.
        let bucket_id = self
            .signature_buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(chat_id)
            .copied();
        let bucket_client = bucket_id.and_then(|id| {
            self.bucket_clients[id]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });

        let mut clients_to_try: Vec<&Client> = Vec::new();
        if let Some(ref bc) = bucket_client {
            clients_to_try.push(bc);
        }
        clients_to_try.push(&self.client);

        let mut last_error = None;
        for client in clients_to_try {
            let response = client
                .get(&canonical_url)
                .headers(headers.clone())
                .timeout(timeout)
                .send()
                .await
                .map_err(|e| CompletionError::CompletionError(format_error_chain(&e)))?;

            if response.status().is_success() {
                let signature = response
                    .json()
                    .await
                    .map_err(|e| CompletionError::CompletionError(format_error_chain(&e)))?;
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

            // Only retry on 404 (wrong backend) — other errors are definitive
            if status != 404 {
                break;
            }
        }

        Err(CompletionError::CompletionError(
            last_error.unwrap_or_else(|| "Signature fetch failed".to_string()),
        ))
    }

    fn pin_chat_connection(&self, request_hash: &str, chat_id: &str) {
        if let Some(bucket_id) = self
            .pending_buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(request_hash)
        {
            self.signature_buckets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(chat_id.to_string(), bucket_id);
        }
        // Streaming attempts that fell over to a rotation index left the
        // index in `pending_rotation` keyed by request_hash; promote it
        // alongside the bucket-side mapping so `get_signature` knows which
        // SNI to use. Empty chat_id (orphan-cleanup case) only clears the
        // pending entry without writing into signature_rotation.
        if let Some(index) = self
            .pending_rotation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(request_hash)
        {
            if !chat_id.is_empty() {
                self.signature_rotation
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(chat_id.to_string(), index);
            }
        }
    }

    fn unpin_chat_connection(&self, chat_id: &str) {
        self.signature_buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(chat_id);
        self.signature_rotation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(chat_id);
    }

    fn set_backend_count(&self, count: usize) {
        self.last_backend_count.store(count, Ordering::Relaxed);
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
        let url = format!("{}/v1/models", self.config.base_url);
        tracing::debug!("Listing models from vLLM server, url: {}", url);

        let headers = self.build_headers().map_err(ListModelsError::FetchError)?;
        let response = self
            .client
            .get(&url)
            .headers(headers)
            .timeout(self.config.control_timeout())
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

    /// Performs a streaming chat completion request
    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        let url = format!("{}/v1/chat/completions", self.config.base_url);

        // Ensure streaming and token usage are enabled
        let mut streaming_params = params;
        streaming_params.stream = Some(true);
        streaming_params.stream_options = Some(StreamOptions {
            include_usage: Some(true),
            continuous_usage_stats: Some(true),
        });

        let mut headers = self
            .build_headers()
            .map_err(CompletionError::CompletionError)?;
        let request_hash_value = HeaderValue::from_str(&request_hash)
            .map_err(|e| CompletionError::CompletionError(format!("Invalid request hash: {e}")))?;
        headers.insert("X-Request-Hash", request_hash_value);

        // Prepare encryption headers
        self.prepare_encryption_headers(&mut headers, &mut streaming_params.extra);

        // Route to a bucket client based on prompt prefix.
        // The bucket client maintains a persistent H2 connection to a verified backend
        // via L4 passthrough → prefix cache hits. Buckets are lazily filled: on first
        // use, inline verification connects to a backend, verifies attestation, and
        // pins the client.
        let bucket_id = self.prefix_router.route(&streaming_params.messages);
        let bucket_client = self.get_or_verify_bucket_client(bucket_id).await?;
        let canonical_send = match self
            .send_streaming_request(
                &url,
                headers.clone(),
                &streaming_params,
                Some(&bucket_client),
            )
            .await
        {
            Ok(r) => Ok(r),
            Err(ref e) if Self::is_connection_error(e) => {
                // Connection dropped or fingerprint mismatch on reconnect —
                // clear bucket and re-verify with a fresh attestation.
                self.clear_bucket(bucket_id);
                let fresh = self.get_or_verify_bucket_client(bucket_id).await?;
                self.send_streaming_request(&url, headers.clone(), &streaming_params, Some(&fresh))
                    .await
            }
            Err(e) => Err(e),
        };

        // Decision tree before exposing the stream:
        //   - HTTP-level 5xx/429 (status arrived in response headers): try
        //     rotation-SNI backends in order.
        //   - HTTP 200 + first SSE chunk is `{"error":{"code":N,...}}`
        //     (SGLang queue-full path, which inference-proxy's SseTransformer
        //     forwards verbatim): peek catches it via the parser's typed
        //     `HttpError` and we route to the same rotation fallback.
        //   - Otherwise: pin bucket, return peekable as the live stream.
        //
        // We only peek when rotation is actually possible
        // (`rotation_count() > 0`): the peek blocks until the first SSE
        // chunk arrives, so on the happy path it adds first-byte latency
        // to every streaming request. When rotation can't help (cold-start
        // before discovery's first cycle, or non-rotation URLs like
        // `localhost`), skip the peek so a first-chunk error still surfaces
        // through the route layer's `sse_error_frame` path (PR #629) and
        // happy-path streams return HTTP 200 as soon as the headers arrive.
        match canonical_send {
            Ok(response) if self.rotation_count() == 0 => {
                self.pending_buckets
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(request_hash, bucket_id);
                let sse_stream = new_sse_parser(response.bytes_stream(), true);
                Ok(Box::pin(sse_stream))
            }
            Ok(response) => {
                let parser = new_sse_parser(response.bytes_stream(), true);
                let stream: StreamingResult = Box::pin(parser);
                let mut peekable = StreamingResultExt::peekable(stream);
                let first_chunk_status =
                    if let Some(Err(CompletionError::HttpError { status_code, .. })) =
                        peekable.peek().await
                    {
                        if Self::is_rotation_retryable_status(*status_code) {
                            Some(*status_code)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                match first_chunk_status {
                    None => {
                        self.pending_buckets
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(request_hash, bucket_id);
                        Ok(Box::pin(peekable))
                    }
                    Some(status_code) => {
                        // rotation_count() > 0 is guaranteed by the arm
                        // guard above, so the fallback will actually iterate
                        // at least one alternative backend.
                        drop(peekable);
                        self.try_chat_completion_stream_rotation(
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
                    if Self::is_rotation_retryable_status(*status_code)
                        && self.rotation_count() > 0 =>
                {
                    self.try_chat_completion_stream_rotation(
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
        let url = format!("{}/v1/chat/completions", self.config.base_url);

        let mut non_streaming_params = params;

        let mut headers = self
            .build_headers()
            .map_err(CompletionError::CompletionError)?;
        let request_hash_value = HeaderValue::from_str(&request_hash)
            .map_err(|e| CompletionError::CompletionError(format!("Invalid request hash: {e}")))?;
        headers.insert("X-Request-Hash", request_hash_value);

        // Prepare encryption headers
        self.prepare_encryption_headers(&mut headers, &mut non_streaming_params.extra);

        // Route to a verified bucket client based on prompt prefix.
        let bucket_id = self.prefix_router.route(&non_streaming_params.messages);
        let bucket_client = self.get_or_verify_bucket_client(bucket_id).await?;
        let timeout_secs = self.config.completion_timeout_seconds.max(0) as u64;
        let timeout = Duration::from_secs(timeout_secs);

        let send = |client: &Client, hdrs: reqwest::header::HeaderMap| {
            client
                .post(&url)
                .headers(hdrs)
                .json(&non_streaming_params)
                .timeout(timeout)
                .send()
        };

        // Distinguish timeout from other transport errors so the pool can refuse
        // to retry timeouts (a re-send hits the same model with the same prompt).
        // Connect-level timeouts are excluded: those usually indicate transient
        // network blips and are worth retrying via the bucket-clear path below.
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

        let response = match send(&bucket_client, headers.clone()).await {
            Ok(r) => r,
            // Connection dropped or fingerprint mismatch on reconnect — clear
            // bucket and re-verify with a fresh attestation. Two subtleties:
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
                self.clear_bucket(bucket_id);
                let fresh = self.get_or_verify_bucket_client(bucket_id).await?;
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
            // The sticky bucket landed on a backend whose queue is full (or
            // is otherwise reporting 5xx/429). Walk each backend by index via
            // model-proxy's rotation SNI before surfacing the error: with H2
            // pooling disabled on the rotation client, every attempt lands
            // on a distinct backend. If one is healthy, the request succeeds
            // and we record the index for signature retrieval.
            if Self::is_rotation_retryable_status(status_code) && self.rotation_count() > 0 {
                return self
                    .try_chat_completion_rotation(
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

        // Store the effective bucket ID for signature fetching.
        // For non-streaming, we know the chat_id immediately.
        let chat_id = chat_completion_response.id.clone();
        self.signature_buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(chat_id, bucket_id);

        Ok(ChatCompletionResponseWithBytes {
            response: chat_completion_response,
            raw_bytes,
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

        // Forward encryption headers from extra to HTTP headers
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
            // Send as JSON array string
            form = form.text("timestamp_granularities[]", granularities.join(","));
        }

        // Build headers (no Content-Type - reqwest sets it automatically for multipart)
        let mut headers = self
            .build_headers()
            .map_err(|e| AudioTranscriptionError::TranscriptionError(e.to_string()))?;
        // Forward encryption headers from extra to HTTP headers
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
            tracing::error!(
                status_code,
                "Audio transcription request failed with HTTP error"
            );
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
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(EmbeddingError::HttpError {
                status_code,
                message,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

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

    fn create_test_provider() -> VLlmProvider {
        VLlmProvider::new(VLlmConfig {
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
            let cfg = VLlmConfig::new("http://x".to_string(), None, None);
            assert_eq!(
                cfg.completion_timeout_seconds,
                VLlmConfig::DEFAULT_COMPLETION_TIMEOUT_SECS
            );
            assert_eq!(
                cfg.control_timeout_seconds,
                VLlmConfig::DEFAULT_CONTROL_TIMEOUT_SECS
            );
            assert_eq!(
                cfg.completion_timeout(),
                Duration::from_secs(VLlmConfig::DEFAULT_COMPLETION_TIMEOUT_SECS as u64)
            );
            assert_eq!(
                cfg.control_timeout(),
                Duration::from_secs(VLlmConfig::DEFAULT_CONTROL_TIMEOUT_SECS as u64)
            );
        });
    }

    #[test]
    #[serial]
    fn vllm_config_reads_env_vars_when_present() {
        with_clean_timeout_env(|| {
            std::env::set_var("VLLM_PROVIDER_COMPLETION_TIMEOUT", "1234");
            std::env::set_var("VLLM_PROVIDER_CONTROL_TIMEOUT", "42");
            let cfg = VLlmConfig::new("http://x".to_string(), None, None);
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
            let cfg = VLlmConfig::new("http://x".to_string(), None, Some(7));
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
            let cfg = VLlmConfig::new("http://x".to_string(), None, None);
            assert_eq!(
                cfg.completion_timeout_seconds,
                VLlmConfig::DEFAULT_COMPLETION_TIMEOUT_SECS
            );
            assert_eq!(
                cfg.control_timeout_seconds,
                VLlmConfig::DEFAULT_CONTROL_TIMEOUT_SECS
            );
        });
    }

    #[test]
    fn vllm_config_negative_timeout_clamped_to_zero_duration() {
        let cfg = VLlmConfig {
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

        provider.prepare_encryption_headers(&mut headers, &mut extra);

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

        provider.prepare_encryption_headers(&mut headers, &mut extra);

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

        provider.prepare_encryption_headers(&mut headers, &mut extra);

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
        provider.prepare_encryption_headers(&mut headers, &mut extra);

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
    fn test_bucket_count_matches_prefix_router() {
        let provider = create_test_provider();
        assert_eq!(
            provider.bucket_clients.len(),
            provider.prefix_router.num_buckets()
        );
    }

    #[test]
    fn test_legacy_provider_eagerly_creates_buckets() {
        // Without a verifier, buckets are eagerly pre-created (legacy path)
        let provider = create_test_provider();
        let guard = provider.bucket_clients[0]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(guard.is_some(), "Legacy provider should pre-create buckets");
    }

    #[test]
    fn test_lazy_buckets_start_empty_with_verifier() {
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

        let provider = VLlmProvider::new_with_verifier(
            VLlmConfig {
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
        let guard = provider.bucket_clients[0]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            guard.is_none(),
            "Verifier-backed provider should start with empty buckets"
        );
    }

    #[tokio::test]
    async fn test_get_or_verify_fills_bucket() {
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

        let provider = VLlmProvider::new_with_verifier(
            VLlmConfig {
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
        assert!(provider.bucket_clients[0].lock().unwrap().is_none());

        // get_or_verify fills it
        let result = provider.get_or_verify_bucket_client(0).await;
        assert!(result.is_ok());
        assert!(provider.bucket_clients[0].lock().unwrap().is_some());

        // Second call returns cached client (fast path)
        let result2 = provider.get_or_verify_bucket_client(0).await;
        assert!(result2.is_ok());
    }

    #[test]
    fn test_clear_bucket() {
        let provider = create_test_provider();
        assert!(provider.bucket_clients[0].lock().unwrap().is_some());
        provider.clear_bucket(0);
        assert!(provider.bucket_clients[0].lock().unwrap().is_none());
    }

    /// Fix 2 + security guard: when a verifier always fails AND no fingerprints
    /// have been pinned yet (Bootstrap state), get_or_verify_bucket_client must
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

        let provider = VLlmProvider::new_with_verifier(
            VLlmConfig {
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
        assert!(provider.bucket_clients[0].lock().unwrap().is_none());
        assert_eq!(provider.pinned_fingerprint_count(), 0);

        // All attempts fail in Bootstrap state → must return Err (not fallback).
        let result = provider.get_or_verify_bucket_client(0).await;
        assert!(
            result.is_err(),
            "expected Err in Bootstrap state, got: {result:?}"
        );

        // Bucket remains empty.
        assert!(provider.bucket_clients[0].lock().unwrap().is_none());
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

        let provider = VLlmProvider::new_with_verifier(
            VLlmConfig {
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
        assert!(provider.bucket_clients[0].lock().unwrap().is_none());

        // All attempts fail but fingerprints are pinned → fallback client returned.
        let result = provider.get_or_verify_bucket_client(0).await;
        assert!(result.is_ok(), "expected fallback Ok, got: {result:?}");

        // Bucket remains empty — fallback is not stored as a verified bucket client.
        assert!(
            provider.bucket_clients[0].lock().unwrap().is_none(),
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

        let provider = VLlmProvider::new_with_verifier(
            VLlmConfig {
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
        assert!(provider.bucket_clients[0].lock().unwrap().is_none());

        // Blocked state has pinned_count == 0 → same safe path as Bootstrap → Err.
        let result = provider.get_or_verify_bucket_client(0).await;
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
        let provider = Arc::new(VLlmProvider::new_with_verifier_and_concurrency(
            VLlmConfig {
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
                p.get_or_verify_bucket_client(0).await
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

        let provider = VLlmProvider::new_with_verifier(
            VLlmConfig {
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

        let start = std::time::Instant::now();
        let result = provider
            .chat_completion(params, "test-hash".to_string())
            .await;
        let elapsed = start.elapsed();

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

        // One timeout cycle is ~1s. A retry would be ~2s. Allow generous
        // headroom for CI scheduler jitter but fail well before 2× to
        // catch the regression.
        assert!(
            elapsed < Duration::from_millis(1700),
            "chat_completion took {elapsed:?} — looks like the bucket-clear retry fired on timeout"
        );
        assert_eq!(
            accept_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one TCP connection should have been opened (no retry)"
        );

        // Drop the acceptor task: this releases the held sockets cleanly so
        // we don't leak file descriptors past the test.
        acceptor.abort();
    }

    /// pre_warm: spawns a background task per bucket that calls
    /// get_or_verify_bucket_client. After awaiting all tasks, every bucket
    /// should be filled and the verifier should have been called exactly once
    /// per bucket (the semaphore double-check prevents duplicate calls for
    /// the same bucket, but each bucket still needs its own client).
    #[tokio::test]
    async fn test_pre_warm_fills_all_buckets() {
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

        let provider = Arc::new(VLlmProvider::new_with_verifier_and_concurrency(
            VLlmConfig {
                base_url: "http://localhost".to_string(),
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
            4, // production-default semaphore concurrency — exercises throttling with 64 tasks
        ));

        let num_buckets = provider.bucket_clients.len();

        // All buckets start empty.
        assert!(provider
            .bucket_clients
            .iter()
            .all(|b| b.lock().unwrap().is_none()));

        // pre_warm fires background tasks — wait for them all to finish.
        provider.clone().pre_warm();
        // Yield repeatedly until every bucket is filled or a generous
        // timeout is exceeded.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let filled = provider
                .bucket_clients
                .iter()
                .filter(|b| b.lock().unwrap().is_some())
                .count();
            if filled == num_buckets {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "pre_warm did not fill all {num_buckets} buckets within timeout; filled={filled}"
            );
            tokio::task::yield_now().await;
        }

        // Every bucket should be filled and the verifier called exactly once
        // per bucket.
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            num_buckets,
            "expected one verification call per bucket"
        );
    }

    /// pre_warm is a no-op when no backend verifier is configured (legacy mode).
    #[tokio::test]
    async fn test_pre_warm_noop_without_verifier() {
        let provider = Arc::new(VLlmProvider::new(VLlmConfig {
            base_url: "http://localhost".to_string(),
            api_key: None,
            completion_timeout_seconds: 30,
            control_timeout_seconds: 30,
        }));

        // In legacy mode buckets are eagerly pre-filled at construction.
        assert!(provider
            .bucket_clients
            .iter()
            .all(|b| b.lock().unwrap().is_some()));

        // pre_warm should not panic and should not clear the pre-filled buckets.
        provider.clone().pre_warm();
        assert!(provider
            .bucket_clients
            .iter()
            .all(|b| b.lock().unwrap().is_some()));
    }

    /// pre_warm is a no-op when no fingerprints are pinned (Bootstrap or Blocked state).
    /// Without this guard, pre_warm would spawn 64 tasks that each fail the security
    /// check in get_or_verify_bucket_client and log spurious warnings.
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
            let provider = Arc::new(VLlmProvider::new_with_verifier_and_concurrency(
                VLlmConfig {
                    base_url: "http://localhost".to_string(),
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
            // All buckets must remain empty (no tasks ran).
            assert!(
                provider
                    .bucket_clients
                    .iter()
                    .all(|b| b.lock().unwrap().is_none()),
                "pre_warm should not fill any buckets in Bootstrap/Blocked state"
            );
        }
    }

    #[test]
    fn rotation_retryable_status_covers_5xx_and_429() {
        // Mirrors `classify_retry_decision` in the pool ("retryable_http_5xx"
        // + 429). Keeping these in sync is load-bearing: if the rotation
        // gate diverges, a 503 that the pool considers retryable could
        // bypass rotation and burn the pool's 3-round backoff against the
        // same overloaded bucket.
        assert!(VLlmProvider::is_rotation_retryable_status(429));
        assert!(VLlmProvider::is_rotation_retryable_status(500));
        assert!(VLlmProvider::is_rotation_retryable_status(503));
        assert!(VLlmProvider::is_rotation_retryable_status(599));
        assert!(!VLlmProvider::is_rotation_retryable_status(200));
        assert!(!VLlmProvider::is_rotation_retryable_status(400));
        assert!(!VLlmProvider::is_rotation_retryable_status(404));
        assert!(!VLlmProvider::is_rotation_retryable_status(408));
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
            provider.rotation_count(),
            0,
            "rotation must stay disabled for URLs that don't fit the <canonical>.<multi-label-base> shape"
        );
        assert!(provider.rotation_url(0, "/v1/chat/completions").is_none());
    }

    #[test]
    fn rotation_url_uses_canonical_label_and_index() {
        let provider = VLlmProvider::new(VLlmConfig {
            base_url: "https://glm-5-1.completions.near.ai".to_string(),
            api_key: None,
            completion_timeout_seconds: 30,
            control_timeout_seconds: 30,
        });
        provider.set_backend_count(3);
        assert_eq!(provider.rotation_count(), 3);
        let url0 = provider
            .rotation_url(0, "/v1/chat/completions")
            .expect("rotation URL build");
        let url2 = provider
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
        let provider = VLlmProvider::new(VLlmConfig {
            base_url: "https://glm-5-1.completions.near.ai".to_string(),
            api_key: None,
            completion_timeout_seconds: 30,
            control_timeout_seconds: 30,
        });
        provider.set_backend_count(10_000);
        assert_eq!(provider.rotation_count(), VLlmProvider::MAX_ROTATION_FANOUT);
    }

    #[test]
    fn rotation_count_returns_zero_when_discovery_has_not_run() {
        // First request after startup, before discovery's first cycle: count
        // is 0, so rotation is skipped and the canonical 5xx propagates
        // as it did pre-this-PR. No false positives.
        let provider = VLlmProvider::new(VLlmConfig {
            base_url: "https://glm-5-1.completions.near.ai".to_string(),
            api_key: None,
            completion_timeout_seconds: 30,
            control_timeout_seconds: 30,
        });
        assert_eq!(provider.rotation_count(), 0);
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
            .pending_rotation
            .lock()
            .unwrap()
            .insert("req-hash-abc".to_string(), 2);
        provider.pin_chat_connection("req-hash-abc", "chatcmpl-xyz");
        let stored = provider
            .signature_rotation
            .lock()
            .unwrap()
            .get("chatcmpl-xyz")
            .copied();
        assert_eq!(stored, Some(2));
        // Pending entry should be drained so a future `request_hash` reuse
        // can't accidentally surface the stale index.
        assert!(provider.pending_rotation.lock().unwrap().is_empty());
    }

    #[test]
    fn pin_chat_connection_with_empty_chat_id_drains_pending_without_writing_signature() {
        // The pool's orphan-cleanup path (`provider.pin_chat_connection(hash, "")`)
        // must drop the pending mapping without leaking an entry under an
        // empty chat_id key — otherwise every orphan request would
        // collide on the same `""` signature_rotation slot.
        let provider = create_test_provider();
        provider
            .pending_rotation
            .lock()
            .unwrap()
            .insert("req-hash-orphan".to_string(), 1);
        provider.pin_chat_connection("req-hash-orphan", "");
        assert!(provider.pending_rotation.lock().unwrap().is_empty());
        assert!(provider.signature_rotation.lock().unwrap().is_empty());
    }

    #[test]
    fn unpin_chat_connection_clears_signature_rotation() {
        let provider = create_test_provider();
        provider
            .signature_rotation
            .lock()
            .unwrap()
            .insert("chat-1".to_string(), 4);
        provider.unpin_chat_connection("chat-1");
        assert!(provider.signature_rotation.lock().unwrap().is_empty());
    }
}
