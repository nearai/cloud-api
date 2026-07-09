//! Chutes — an attested third-party inference provider (`ProviderTier::Attested3p`).
//!
//! Chutes (chutes.ai) serves models from Intel TDX + NVIDIA confidential-compute
//! TEEs. Unlike NEAR's external providers, its verified data path is **not** plain
//! TLS to `llm.chutes.ai` (that gateway terminates TLS at a CA cert, not the
//! attested key). Instead every inference request runs the full attested flow:
//!
//! 1. resolve model → `chute_id`; discover live instances + their ML-KEM-768
//!    `e2e_pubkey` and single-use nonce tokens ([`client`]);
//! 2. fetch `/evidence` for a fresh boot nonce and **verify the chosen instance**
//!    end-to-end via the injected [`verifier_port::ChutesInstanceVerifier`] (TDX
//!    quote + `report_data` bindings + register-pin measurement + GPU). A failure
//!    is **fatal** — we never fall back to an unverified channel;
//! 3. ML-KEM-encapsulate the OpenAI request to that instance's attested
//!    `e2e_pubkey` and `POST /e2e/invoke` ([`e2ee`]); only the attested instance
//!    can decrypt it, so the response is cryptographically bound to verified
//!    software even through the load-balancing gateway.
//!
//! The verifier lives in the `services` crate (it needs DCAP + NRAS); this
//! provider reaches it through the [`verifier_port`] seam, which `services`
//! implements and the provider pool injects.
//!
//! Only chat is implemented (Chutes' TEE models are chat models); other
//! modalities return an explicit "unsupported" error rather than exposing an
//! unattested path. Turning Chutes on is gated behind an enable flag in the pool.

pub mod attestation;
mod availability;
pub mod client;
pub mod e2ee;
pub mod e2ee_stream;
pub mod evidence;
pub mod measurements;
pub mod report_data;
pub mod verifier_port;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use futures_util::StreamExt;
use serde_json::{json, Value};

use self::client::{ChutesClient, ChutesClientError, InvokeMode, InvokeRequest};
use self::verifier_port::ChutesInstanceVerifier;
use crate::{
    AttestationError, AudioTranscriptionError, AudioTranscriptionParams,
    AudioTranscriptionResponse, ChatCompletionParams, ChatCompletionResponse,
    ChatCompletionResponseWithBytes, ChatSignature, CompletionError, CompletionParams,
    EmbeddingError, ImageEditError, ImageEditParams, ImageEditResponseWithBytes,
    ImageGenerationError, ImageGenerationParams, ImageGenerationResponseWithBytes,
    InferenceProvider, ListModelsError, ModelInfo, ModelsResponse, PrivacyClassifyError,
    RerankError, RerankParams, RerankResponse, SSEEvent, ScoreError, ScoreParams, ScoreResponse,
    StreamChunk, StreamingResult,
};

/// Sane fallback when a non-positive timeout is supplied; a non-positive value
/// would otherwise underflow the `as u64` cast in the HTTP layer.
const DEFAULT_TIMEOUT_SECONDS: i64 = 300;

/// The OpenAI sub-path invoked inside the chute for chat completions.
const CHAT_PATH: &str = "/v1/chat/completions";

/// Configuration for a Chutes attested provider.
///
/// `api_key` is private with a redacting `Debug` so the `cpk_...` secret can
/// never leak via `{:?}`. The golden measurements + DCAP config live with the
/// verifier (in `services`), injected separately.
#[derive(Clone)]
pub struct Config {
    /// Chutes API key (`cpk_...`) — a secret; sourced from env/secret store.
    api_key: String,
    /// The model id as served by Chutes — the chute SLUG (e.g. `zai-org/GLM-5.1-TEE`).
    /// Sent upstream and resolved to a `chute_id`; NEVER surfaced to clients.
    model_name: String,
    /// The CANONICAL id we expose to clients and rewrite `response.model` to (e.g.
    /// `zai-org/GLM-5.1-FP8`, or the OpenRouter id). Defaults to `model_name` (the
    /// slug) when not set via [`Config::with_canonical_id`]; when it differs, the
    /// decrypted response's `model` is rewritten so it never leaks the slug.
    canonical_id: String,
    /// Per-request timeout, seconds. Always > 0 (see `new`).
    timeout_seconds: i64,
    /// Optional host overrides (tests/staging). Both or neither.
    api_base: Option<String>,
    models_base: Option<String>,
    /// Expose the streaming chat path as attested. Default `false` — streaming
    /// has no authenticated frame ordering (see [`e2ee_stream`]); the honest
    /// attested default is non-streaming. Opt in via `Config::with_streaming`.
    allow_streaming: bool,
}

impl Config {
    /// Build a config. A non-positive `timeout_seconds` is replaced with
    /// [`DEFAULT_TIMEOUT_SECONDS`].
    pub fn new(api_key: String, model_name: String, timeout_seconds: i64) -> Self {
        Self {
            api_key,
            canonical_id: model_name.clone(),
            model_name,
            timeout_seconds: if timeout_seconds > 0 {
                timeout_seconds
            } else {
                DEFAULT_TIMEOUT_SECONDS
            },
            api_base: None,
            models_base: None,
            allow_streaming: false,
        }
    }

    /// Set the canonical id surfaced to clients (the NEAR-served id, or the
    /// OpenRouter id) when it differs from the chute slug. `response.model` is
    /// rewritten to this so the raw `-TEE` slug never reaches a client.
    pub fn with_canonical_id(mut self, canonical_id: impl Into<String>) -> Self {
        self.canonical_id = canonical_id.into();
        self
    }

    /// Override the Chutes hosts (`api.chutes.ai` / `llm.chutes.ai`) for tests or
    /// staging.
    pub fn with_hosts(
        mut self,
        api_base: impl Into<String>,
        models_base: impl Into<String>,
    ) -> Self {
        self.api_base = Some(api_base.into());
        self.models_base = Some(models_base.into());
        self
    }

    /// Opt into exposing the streaming chat path as attested (default off — see
    /// [`Config::allow_streaming`]).
    pub fn with_streaming(mut self, allow: bool) -> Self {
        self.allow_streaming = allow;
        self
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    pub fn timeout_seconds(&self) -> i64 {
        self.timeout_seconds
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("api_key", &"[redacted]")
            .field("model_name", &self.model_name)
            .field("timeout_seconds", &self.timeout_seconds)
            .field("api_base", &self.api_base)
            .field("models_base", &self.models_base)
            .finish()
    }
}

/// Chutes attested inference provider. Every chat request is served over a
/// verified ML-KEM-768 E2EE channel to an attested instance.
pub struct Provider {
    client: ChutesClient,
    verifier: Arc<dyn ChutesInstanceVerifier>,
    model_name: String,
    /// Canonical id surfaced to clients; `response.model` is rewritten to this
    /// when it differs from `model_name` (the chute slug). See [`Config::canonical_id`].
    canonical_id: String,
    /// Whether the streaming chat path is exposed as attested (see
    /// [`Config::allow_streaming`]).
    allow_streaming: bool,
    /// Memoized model→`chute_id` (the mapping is static), so we don't re-fetch
    /// `/v1/models` on every request.
    chute_id_cache: tokio::sync::OnceCell<String>,
    /// Short-TTL cache of `/e2e/instances` discovery PER chute (#774). One
    /// discovery response carries ~5 instances × ~10 single-use nonce tokens
    /// (≈50 tokens) valid for `nonce_expires_in` seconds, so caching it and
    /// consuming nonces one-at-a-time serves ~50 requests from a single
    /// rate-limited discovery call — cutting the call rate that self-inflicts the
    /// `/e2e/instances` 429s.
    ///
    /// The OUTER `std::sync::Mutex` guards ONLY a fast get-or-create of the
    /// per-chute cell (never held across an `await`). Each chute's
    /// `Arc<tokio::sync::Mutex<CachedInstances>>` serializes refresh + nonce
    /// consumption for THAT chute only (single-flight per chute; other chutes are
    /// unaffected). ATTESTATION IS UNCHANGED — only `/e2e/instances` discovery is
    /// cached; every request still mints a fresh boot nonce, fetches `/evidence`,
    /// and verifies the chosen instance.
    instances_cache: Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, Arc<tokio::sync::Mutex<CachedInstances>>>,
        >,
    >,
}

/// A cached `/e2e/instances` discovery snapshot for one chute. `instances` is the
/// owned discovery result; nonce tokens are POPPED from it as they're consumed
/// (single-use), so the same token is never handed out twice. `expires_at` is the
/// freshness deadline derived from the upstream `nonce_expires_in` (with a safety
/// margin); past it — or once an instance's nonce pool empties — the next use
/// refreshes via a fresh discovery call.
struct CachedInstances {
    instances: Vec<client::E2eInstance>,
    expires_at: std::time::Instant,
}

impl CachedInstances {
    /// An empty, already-expired entry: its first use triggers a refresh (the
    /// `or_insert_with` seed for a chute we haven't discovered yet).
    fn empty_expired() -> Self {
        Self {
            instances: Vec::new(),
            expires_at: std::time::Instant::now(),
        }
    }
}

/// Everything needed to invoke a verified instance: the targeting headers, the
/// E2EE request blob, and the session to decrypt the reply.
struct PreparedInvoke {
    chute_id: String,
    instance_id: String,
    nonce_token: String,
    blob: Vec<u8>,
    session: e2ee::ResponseSession,
}

impl Provider {
    /// Build a provider. `verifier` is the `services`-side attestation verifier,
    /// injected through the [`verifier_port`] seam.
    pub fn new(
        config: Config,
        verifier: Arc<dyn ChutesInstanceVerifier>,
    ) -> Result<Self, ChutesClientError> {
        let timeout = config.timeout_seconds.max(1) as u64;
        let allow_streaming = config.allow_streaming;
        let mut client = ChutesClient::new(config.api_key, timeout)?;
        if let (Some(api), Some(models)) = (config.api_base, config.models_base) {
            client = client.with_hosts(api, models);
        }
        Ok(Self {
            client,
            verifier,
            model_name: config.model_name,
            canonical_id: config.canonical_id,
            allow_streaming,
            chute_id_cache: tokio::sync::OnceCell::new(),
            instances_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        })
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    fn random_boot_nonce() -> Result<String, String> {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).map_err(|e| format!("OS RNG unavailable: {e}"))?;
        Ok(hex::encode(b))
    }

    /// Resolve `model_name` → `chute_id`, memoized: the mapping is static, so the
    /// `/v1/models` lookup happens once per provider. Shared by the data path
    /// (`verify_and_prepare`) and the attestation-report path so they can't
    /// diverge (and the report path doesn't re-hit the network each call).
    ///
    /// Returns the typed [`client::ChutesClientError`] so each caller maps it
    /// appropriately: the data path routes it through [`Self::map_client_error`]
    /// (preserving a `/v1/models` 429 as retryable — `OnceCell` doesn't cache
    /// failures, so a retry genuinely re-resolves), while the attestation-report
    /// path keeps its own `AttestationError::FetchError` formatting.
    async fn cached_chute_id(&self) -> Result<String, client::ChutesClientError> {
        self.chute_id_cache
            .get_or_try_init(|| self.client.resolve_chute_id(&self.model_name))
            .await
            .cloned()
    }

    /// Get-or-create the per-chute discovery cache cell. Holds the OUTER
    /// `std::sync::Mutex` only for the `entry().or_insert_with().clone()` — a
    /// non-blocking map op, never across an `await` — then returns the cell so the
    /// caller can lock its own per-chute `tokio::sync::Mutex` for the (awaiting)
    /// refresh / nonce consumption. `unwrap_or_else(|e| e.into_inner())` recovers
    /// the map even if a thread panicked while holding it (a poisoned outer lock
    /// would otherwise wedge discovery for every chute).
    fn chute_cache(&self, chute_id: &str) -> Arc<tokio::sync::Mutex<CachedInstances>> {
        let mut map = self
            .instances_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.entry(chute_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(CachedInstances::empty_expired())))
            .clone()
    }

    /// Return a fresh-enough `/e2e/instances` snapshot for `chute_id`, refreshing
    /// via a real discovery call only when the cached entry is expired or carries
    /// no usable (E2E-capable + nonce-bearing) instance.
    ///
    /// SINGLE-FLIGHT PER CHUTE: the per-chute `tokio::sync::Mutex` is held across
    /// the refresh, so concurrent requests for the SAME chute wait and then observe
    /// the freshly-cached snapshot (no thundering herd onto the rate-limited
    /// endpoint); requests for OTHER chutes never block (each has its own cell).
    /// Returns an OWNED snapshot (`instances.clone()`) for candidate selection +
    /// verification, releasing the lock after — nonce consumption happens later via
    /// [`Self::take_nonce`] (the atomic single-use point).
    async fn discover_cached(
        &self,
        chute_id: &str,
    ) -> Result<Vec<client::E2eInstance>, CompletionError> {
        let cell = self.chute_cache(chute_id);
        let mut guard = cell.lock().await;
        let usable = guard
            .instances
            .iter()
            .any(|i| !i.e2e_pubkey.is_empty() && !i.nonces.is_empty());
        if guard.expires_at <= std::time::Instant::now() || !usable {
            let fresh = self
                .client
                .discover_instances(chute_id)
                .await
                .map_err(|e| Self::map_client_error("discover instances", e))?;
            // TTL from the upstream nonce lifetime, clamped to a sane ceiling and
            // shaved by a safety margin so we refresh BEFORE the live nonces would
            // expire mid-flight (a stale nonce would 4xx the invoke). A missing /
            // non-positive value falls back to a conservative 30s.
            let ttl = fresh
                .nonce_expires_in
                .filter(|s| *s > 0)
                .map(|s| (s as u64).min(120))
                .unwrap_or(30);
            *guard = CachedInstances {
                instances: fresh.instances,
                expires_at: std::time::Instant::now()
                    + std::time::Duration::from_secs(ttl)
                        .saturating_sub(std::time::Duration::from_secs(5)),
            };
        }
        Ok(guard.instances.clone())
    }

    /// Atomically consume one single-use nonce token for `instance_id` from the
    /// cached snapshot (the point that PREVENTS reuse: a popped token is gone from
    /// the cache, so no concurrent request can hand out the same one). Returns
    /// `None` if the instance is no longer present or its pool is already drained —
    /// the caller then moves to the next candidate (and a fully drained chute
    /// refreshes on the next [`Self::discover_cached`]).
    async fn take_nonce(&self, chute_id: &str, instance_id: &str) -> Option<String> {
        let cell = self.chute_cache(chute_id);
        let mut guard = cell.lock().await;
        let inst = guard
            .instances
            .iter_mut()
            .find(|i| i.instance_id == instance_id)?;
        inst.nonces.pop()
    }

    /// Map a Chutes HTTP-client error to a `CompletionError`. Only statuses that
    /// the pool's classifier treats as RETRYABLE or correctly masks are preserved
    /// as `HttpError { status_code, is_external: true }`:
    ///
    /// * `429` → `RateLimitExceeded`, retried across rounds with rate-limit
    ///   backoff (`retryable_http_429`) — the headline `/e2e/instances`
    ///   rate-limit case this fix targets;
    /// * `5xx` → masked `ProviderError 502` / `ServiceOverloaded` (503), retried
    ///   across rounds with backoff;
    /// * `408` → `ProviderError 504`. NOTE: the pool labels this
    ///   `non_retryable_http_408` — the next provider in the *same* round is
    ///   tried, but the round itself is not retried with backoff (only 5xx/429
    ///   retry the round). Preserving 408 still beats the prior flat 502 because
    ///   it surfaces the timeout and enables that same-round fallthrough.
    ///
    /// Every other status — notably `400 / 413 / 422` — is deliberately collapsed
    /// to a generic `CompletionError(msg)` (which masks as a 502). Discovery /
    /// evidence / invoke requests are internally constructed, so a 4xx there is
    /// never the customer's fault; preserving it would hit `map_provider_error`'s
    /// `InvalidParams` arms and leak the stage label, provider name, and raw
    /// upstream body to the client as a misattributed HTTP 400. This keeps the
    /// retryability win with no client-facing contract change. `ctx` labels the
    /// failing stage for logs/messages.
    fn map_client_error(ctx: &str, e: client::ChutesClientError) -> CompletionError {
        let msg = format!("{ctx}: {e}");
        match e {
            client::ChutesClientError::Status { status: 400, body }
                if availability::stale_invoke_target(ctx, &body) =>
            {
                availability::retryable_provider_unavailable(ctx, "stale Chutes E2E target")
            }
            // Retryable / correctly-masked upstream statuses: preserve so the
            // pool classifier can act on them. The message is the STAGE + STATUS
            // only — NOT the raw upstream body (#778 follow-up): a 5xx body could
            // contain keywords (e.g. "image"/"media") that trip the pool's
            // `is_client_media_fetch_error` substring scan and get misclassified as
            // a client error. The stage + status is all the classifier and logs
            // need; the masked arm below keeps its full `msg` (no status carried,
            // so no such scan applies).
            client::ChutesClientError::Status {
                status: status @ (408 | 429 | 500..=599),
                ..
            } => CompletionError::HttpError {
                status_code: status,
                message: format!("{ctx}: Chutes returned HTTP {status}"),
                is_external: true,
            },
            client::ChutesClientError::Http(_) => {
                availability::retryable_provider_unavailable(ctx, "Chutes HTTP transport error")
            }
            // Any other upstream status (400/413/422/…) on an internally-built
            // request, plus all non-status client errors (transport / oversized
            // body / model-not-found / missing-chute-id / decode), mask as the
            // prior generic 502. Listed explicitly (no `_`) so a new
            // `ChutesClientError` variant forces this mapping to be revisited.
            client::ChutesClientError::Status { .. }
            | client::ChutesClientError::BodyTooLarge { .. }
            | client::ChutesClientError::ModelNotFound(_)
            | client::ChutesClientError::MissingChuteId(_)
            | client::ChutesClientError::Decode { .. } => CompletionError::CompletionError(msg),
        }
    }

    /// Discover → fetch evidence → **verify** a Chutes instance, then build the
    /// E2EE request blob for `request_json`. Returns an error (never an
    /// unverified channel) if any stage fails. Discovery / evidence / invoke
    /// failures preserve the upstream HTTP status (via [`Self::map_client_error`]),
    /// so a rate-limit 429 is retryable rather than a flat 502.
    async fn verify_and_prepare(
        &self,
        request_json: &Value,
    ) -> Result<PreparedInvoke, CompletionError> {
        // Cached: the model→chute_id mapping is static, so resolve once. A
        // `/v1/models` rate-limit 429 here is preserved as retryable too.
        let chute_id = self
            .cached_chute_id()
            .await
            .map_err(|e| Self::map_client_error("resolve chute_id", e))?;

        // Cached `/e2e/instances` discovery (#774): a short-TTL snapshot serves
        // ~50 requests per discovery call (single-flight per chute), cutting the
        // call rate that self-inflicts the 429s. `instances` is an OWNED snapshot;
        // nonces are consumed later via `take_nonce` (the single-use atomic point).
        let instances = self.discover_cached(&chute_id).await?;

        // Candidate instances: live + E2E-capable + with at least one nonce token.
        let candidates: Vec<&client::E2eInstance> = instances
            .iter()
            .filter(|i| !i.e2e_pubkey.is_empty() && !i.nonces.is_empty())
            .collect();
        if candidates.is_empty() {
            return Err(availability::retryable_provider_unavailable(
                "discover instances",
                "no E2E-capable instance with an available nonce token",
            ));
        }

        // One chute-wide /evidence fetch bound to a fresh boot nonce; every
        // instance's report_data binds this same nonce + its own e2e_pubkey.
        let boot_nonce = Self::random_boot_nonce().map_err(CompletionError::CompletionError)?;
        let evidence_resp = self
            .client
            .fetch_evidence(&chute_id, &boot_nonce)
            .await
            .map_err(|e| Self::map_client_error("fetch evidence", e))?;

        // Try each candidate until one verifies, so a single bad/unverifiable
        // instance doesn't take down all requests. Verification failure is never
        // a fallback to an unverified channel — we just move to the next attested
        // candidate, and fail if none verify.
        //
        // Rotate the *starting* candidate (wrapping counter) so traffic from one
        // gateway process doesn't always hot-spot the first-listed instance —
        // `X-Instance-Id` pinning prevents Chutes-side rebalancing, so we spread
        // it here. Every candidate is still attempted, just in a rotated order.
        let n = candidates.len();
        let start = {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static INSTANCE_RR: AtomicUsize = AtomicUsize::new(0);
            INSTANCE_RR.fetch_add(1, Ordering::Relaxed) % n
        };
        let mut last_err = String::from("no candidate instances");
        let mut last_err_retryable = false;
        for off in 0..n {
            let inst = candidates[(start + off) % n];
            let evidence = match evidence_resp.instance(&inst.instance_id) {
                Some(e) => e,
                None => {
                    last_err = format!("instance {} not present in /evidence", inst.instance_id);
                    last_err_retryable = true;
                    continue;
                }
            };
            // Canonicalize the e2e_pubkey once (trim), and use the SAME string for
            // both the attestation binding (hashed) and the E2EE encapsulation
            // (base64-decoded), so the two can't diverge.
            let e2e_pubkey = inst.e2e_pubkey.trim();
            let info = match self
                .verifier
                .attest_instance(evidence, &boot_nonce, e2e_pubkey)
                .await
            {
                Ok(info) => info,
                Err(e) => {
                    last_err = format!("instance {} attestation failed: {e}", inst.instance_id);
                    last_err_retryable = false;
                    continue;
                }
            };
            let e2e_pk = match base64::engine::general_purpose::STANDARD.decode(e2e_pubkey) {
                Ok(pk) => pk,
                Err(e) => {
                    last_err = format!("instance {} e2e_pubkey base64: {e}", inst.instance_id);
                    last_err_retryable = false;
                    continue;
                }
            };
            let prepared = match e2ee::build_request(&e2e_pk, request_json) {
                Ok(p) => p,
                Err(e) => {
                    last_err = format!("instance {} E2EE build: {e}", inst.instance_id);
                    last_err_retryable = false;
                    continue;
                }
            };
            let e2ee::PreparedRequest { blob, session } = prepared;
            // Consume a single-use nonce token from the CACHE (not from the local
            // snapshot's `inst.nonces`), so it can never be handed to a concurrent
            // request for the same instance (#774). A token drained between the
            // snapshot and here just moves us to the next candidate; if every
            // candidate's pool is drained the request fails (retryable), and the
            // next request's `discover_cached` refreshes the now-empty chute.
            let nonce = match self.take_nonce(&chute_id, &inst.instance_id).await {
                Some(n) => n,
                None => {
                    last_err = format!("instance {} nonce pool drained", inst.instance_id);
                    last_err_retryable = true;
                    continue;
                }
            };
            // IDs only (privacy-safe): which attested instance + vetted config
            // served the request, so an operator can trace it during an incident.
            tracing::info!(
                instance_id = %inst.instance_id,
                measurement_config = %info.measurement_config,
                gpu_verdict = %info.gpu_verdict,
                "Chutes instance verified; routing request"
            );
            return Ok(PreparedInvoke {
                chute_id,
                instance_id: inst.instance_id.clone(),
                nonce_token: nonce,
                blob,
                session,
            });
        }
        if last_err_retryable {
            return Err(availability::retryable_provider_unavailable(
                "verify Chutes instance",
                &last_err,
            ));
        }
        Err(CompletionError::CompletionError(format!(
            "all candidate Chutes instances failed (refusing to send inference); last: {last_err}"
        )))
    }
}

/// Internal `extra` keys that must never reach Chutes (a third party): the
/// tracing identifiers and the client-facing-E2EE markers. `ChatCompletionParams`
/// flattens `extra` into the top-level body, so these would otherwise leak.
const INTERNAL_KEYS: &[&str] = {
    use crate::attested::nearai::{encryption_headers as eh, tracing_headers as th};
    &[
        th::REQUEST_ID,
        th::ORG_ID,
        th::WORKSPACE_ID,
        eh::SIGNING_ALGO,
        eh::CLIENT_PUB_KEY,
        eh::MODEL_PUB_KEY,
        eh::ENCRYPTION_VERSION,
        eh::ENCRYPT_ALL_FIELDS,
    ]
};

/// Standard OpenAI chat-response / chunk fields kept at the TOP LEVEL, plus the two
/// extras this crate deliberately passes through. This is an ALLOWLIST (#780 →
/// #781 follow-up): model-specific serving internals vary too much to denylist
/// reliably (e.g. kimi-k2.6 emits `templated_prompt`, `prompt_token_ids`,
/// `prompt_logprobs`, `kv_transfer_params`, … alongside the already-known
/// `prompt_sha256`/`template_sha256`/`metadata`), so anything NOT on this list is
/// dropped by default.
///
/// Kept:
/// - the standard OpenAI top-level fields — incl. `system_fingerprint` and
///   `service_tier`, which ARE standard (do NOT confuse them with serving internals);
/// - `chutes_verification` — the attestation receipt, deliberately surfaced;
/// - `hidden_states` — the intentional unmodeled passthrough.
const ALLOWED_TOP_LEVEL_FIELDS: &[&str] = &[
    "id",
    "object",
    "created",
    "model",
    "choices",
    "usage",
    "system_fingerprint",
    "service_tier",
    "chutes_verification",
    "hidden_states",
];

/// Standard OpenAI fields kept on each element of `choices` (both the non-stream
/// `message`-bearing shape and the stream `delta`-bearing shape). Anything else
/// (e.g. sglang's `matched_stop`, `token_ids`, `stop_reason`) is dropped. `message`
/// and `delta` are recursed into via [`ALLOWED_MESSAGE_FIELDS`].
const ALLOWED_CHOICE_FIELDS: &[&str] = &["index", "delta", "message", "finish_reason", "logprobs"];

/// Standard OpenAI fields kept on a `choices[].message` (non-stream) or
/// `choices[].delta` (stream). Drops model-specific message internals (e.g. a
/// delta-nested `matched_stop`/`token_ids`). `reasoning` and `reasoning_content`
/// are BOTH kept (some models emit one, some the other, some both — we never drop a
/// legit reasoning field).
const ALLOWED_MESSAGE_FIELDS: &[&str] = &[
    "role",
    "content",
    "reasoning_content",
    "reasoning",
    "tool_calls",
    "function_call",
    "refusal",
    "annotations",
    "name",
    "tool_call_id",
];

/// Drop from `obj` every key that is NOT on `allowed`, in place.
fn retain_allowed(obj: &mut serde_json::Map<String, Value>, allowed: &[&str]) {
    obj.retain(|k, _| allowed.contains(&k.as_str()));
}

/// Sanitize a decrypted response object to the standard OpenAI shape by ALLOWLIST
/// (#780 → #781 follow-up), in place: keep only [`ALLOWED_TOP_LEVEL_FIELDS`] at the
/// top level, [`ALLOWED_CHOICE_FIELDS`] on each `choices[]` element, and
/// [`ALLOWED_MESSAGE_FIELDS`] on each `choices[].message` / `choices[].delta`.
/// Everything else — known internals (`prompt_sha256`, `template_sha256`,
/// `metadata`, `matched_stop`, …) AND unknown future internals — is dropped, so a
/// Chutes response matches the clean shape returned for first-party / Anthropic /
/// OpenAI models. The deliberately-kept extras `chutes_verification` (attestation
/// receipt) and `hidden_states` (passthrough) are on the top-level allowlist and
/// survive untouched. Takes the object map directly — the caller has already
/// established it's a JSON object (a non-object body is kept verbatim). Used on both
/// non-stream paths and per `data:` chunk on the stream path.
///
/// Conservative under-strip: when in doubt a field is added to the allowlist
/// (keeping a stray field is safer than dropping a legitimate one), but the known
/// serving internals above are deliberately absent and therefore dropped.
fn strip_internal_response_fields(obj: &mut serde_json::Map<String, Value>) {
    retain_allowed(obj, ALLOWED_TOP_LEVEL_FIELDS);
    if let Some(choices) = obj.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices {
            if let Some(choice_obj) = choice.as_object_mut() {
                retain_allowed(choice_obj, ALLOWED_CHOICE_FIELDS);
                // Recurse into the message (non-stream) and delta (stream) shapes.
                for inner in ["message", "delta"] {
                    if let Some(m) = choice_obj.get_mut(inner).and_then(Value::as_object_mut) {
                        retain_allowed(m, ALLOWED_MESSAGE_FIELDS);
                    }
                }
            }
        }
    }
}

/// Gate the `usage` field of a streamed chunk object to the OpenAI spec (#781 L1),
/// in place. Per spec, intermediate chunks carry `usage: null`; `usage` appears only
/// on the FINAL chunk, and only when the request set `stream_options.include_usage`.
/// We force `include_usage: true` upstream so billing always sees usage (see
/// [`request_body`]), so this gates only what reaches the CLIENT:
///
/// - a chunk with a non-empty `choices` array is an INTERMEDIATE content chunk → its
///   `usage` is always stripped (covers vLLM's `continuous_usage_stats`, which would
///   otherwise populate `usage` on every chunk);
/// - a chunk with an empty `choices` array is the FINAL usage-only chunk → its
///   `usage` is kept iff `include_usage` was requested, else stripped.
///
/// Returns `true` when the chunk was the FINAL usage-only chunk and the client did
/// NOT request usage: OpenAI emits no final usage chunk at all in that case, so the
/// caller must suppress the whole chunk from the client stream rather than forward
/// a gutted `choices: []` husk (strict SDK parsers reject it, and cost-tracking
/// clients read it as zero usage) — see [`rewrite_sse_event_model`].
///
/// NOTE: this gates only `raw_bytes` (the bytes the passthrough route forwards to the
/// client). The parsed `chunk.usage` is left intact so `InterceptStream` can still
/// read it for billing/limits — see [`rewrite_sse_event_model`].
fn gate_stream_usage(obj: &mut serde_json::Map<String, Value>, include_usage: bool) -> bool {
    if !obj.contains_key("usage") {
        return false;
    }
    let is_final = obj
        .get("choices")
        .and_then(Value::as_array)
        .is_none_or(|c| c.is_empty());
    if !is_final || !include_usage {
        obj.remove("usage");
    }
    is_final && !include_usage
}

/// Rewrite the `model` field of a decrypted OpenAI SSE event to the canonical id
/// (when `canonical` is `Some`), sanitize the Chutes response shape to the standard
/// OpenAI allowlist (#780 → #781), and gate the streamed `usage` field (#781 L1) —
/// in BOTH `raw_bytes` (the byte-exact passthrough path) AND the parsed `chunk` (the
/// route re-serializes from `chunk`, not `raw_bytes`, on the auto-redact /
/// alias-served paths — so leaving the slug/internal fields there would still leak
/// them).
///
/// The parsed chunk is re-derived from the already-sanitized `raw_bytes` rather than
/// patched field-by-field: that guarantees the chunk and `raw_bytes` carry the exact
/// same allowlisted shape, and — crucially — also drops TYPED internal slots
/// (`prompt_token_ids`, `modality`, per-choice `token_ids`) that a `chunk.extra`-only
/// strip would miss (they aren't in the flatten catch-all). The one deliberate
/// difference: the chunk is re-derived from the sanitized value BEFORE the usage
/// gate runs, so it keeps `usage` for `InterceptStream` billing; only the
/// client-facing `raw_bytes` honor the usage gate (see [`gate_stream_usage`]).
///
/// Only touches chunk-bearing data lines; control events ([DONE], blanks, the keyed
/// init) have no chunk and pass through unchanged. We ALWAYS round-trip a
/// chunk-bearing line through the JSON sanitizer rather than guarding with a
/// substring scan: this is a privacy-critical control and a unicode-escaped key
/// (e.g. `"prompt_sha256"`) would defeat a literal-substring fast path while still
/// parsing into `extra`. On any parse/sanitize failure the event is returned as-is
/// (never drop a chunk over a rewrite). The rewrite is ATOMIC: `raw_bytes` is
/// computed first and the parsed chunk is replaced only once that succeeds.
fn rewrite_sse_event_model(
    mut ev: SSEEvent,
    canonical: Option<&str>,
    include_usage: bool,
) -> SSEEvent {
    if ev.chunk.is_none() {
        return ev;
    }
    let Ok(s) = std::str::from_utf8(&ev.raw_bytes) else {
        return ev;
    };
    let content = s.strip_prefix("data:").map(str::trim).unwrap_or(s.trim());
    // Sanitize + canonical-rewrite the value once; reuse it for both the
    // client-facing `raw_bytes` (usage-gated) and the billing-facing parsed chunk
    // (usage preserved).
    let Ok(mut v) = serde_json::from_slice::<Value>(content.as_bytes()) else {
        return ev;
    };
    {
        let Some(obj) = v.as_object_mut() else {
            return ev;
        };
        if let Some(canonical) = canonical {
            obj.insert("model".to_string(), Value::String(canonical.to_string()));
        }
        strip_internal_response_fields(obj);
    }

    // Re-derive the parsed chunk from the sanitized value, BEFORE usage-gating, so
    // it keeps `usage` for `InterceptStream` billing while carrying the allowlisted
    // (internals-stripped, canonical-id) shape the re-serialized route paths emit.
    if let Ok(c) = serde_json::from_value::<crate::ChatCompletionChunk>(v.clone()) {
        ev.chunk = Some(StreamChunk::Chat(c));
    }

    // Client-facing `raw_bytes`: apply the streamed-usage gate (#781 L1). The parsed
    // chunk above is untouched by this gate, so billing still sees `usage`.
    let mut suppress_client_chunk = false;
    if let Some(obj) = v.as_object_mut() {
        suppress_client_chunk = gate_stream_usage(obj, include_usage);
    }
    if suppress_client_chunk {
        // The FINAL usage-only chunk, with the client NOT having requested usage:
        // OpenAI sends no such chunk at all, so emit nothing to the client instead
        // of a gutted `choices: []` husk (strict SDK parsers choke on it, and
        // cost-tracking clients read the stream as zero usage). The parsed chunk
        // is kept so `InterceptStream` still sees `usage` for billing/limits.
        return SSEEvent {
            raw_bytes: bytes::Bytes::new(),
            chunk: ev.chunk,
            raw_passthrough: ev.raw_passthrough,
        };
    }
    let Ok(json) = serde_json::to_string(&v) else {
        return ev;
    };
    SSEEvent {
        raw_bytes: bytes::Bytes::from(format!("data: {json}\n\n")),
        chunk: ev.chunk,
        raw_passthrough: ev.raw_passthrough,
    }
}

/// Transform a decrypted OpenAI JSON body: optionally set `model` to `canonical`,
/// then sanitize the Chutes response shape to the standard OpenAI allowlist (#780 →
/// #781). The deliberately-kept extras `chutes_verification` and `hidden_states`
/// survive (they're on the allowlist). Returns the re-serialized bytes, or `None`
/// if the body isn't a JSON object or (de)serialization fails.
fn transform_response_json(bytes: &[u8], canonical: Option<&str>) -> Option<Vec<u8>> {
    let mut v: Value = serde_json::from_slice(bytes).ok()?;
    let obj = v.as_object_mut()?;
    if let Some(canonical) = canonical {
        obj.insert("model".to_string(), Value::String(canonical.to_string()));
    }
    strip_internal_response_fields(obj);
    serde_json::to_vec(&v).ok()
}

/// Split a string that BEGINS with a `<think>…</think>` block into
/// `(reasoning, remaining_content)`. Returns `None` (leave the content untouched)
/// unless the string — after any leading whitespace — starts with a literal
/// `<think>` open tag AND has a matching `</think>` close tag. The reasoning is the
/// text between the tags; the remaining content is everything after `</think>` with
/// the leading whitespace that separated the block from the answer trimmed off.
///
/// Robustness contract (issue #779):
/// - no `<think>` at the very start (e.g. answer text first, or no tag) → `None`;
/// - an *unclosed* / malformed `<think>` with no `</think>` → `None` (we never lose
///   content by guessing where reasoning ends);
/// - a well-formed block at the start → `Some((reasoning, answer))`, where `answer`
///   may be empty (a response that is reasoning-only).
///
/// We deliberately only handle the block-at-the-very-start case: that is exactly
/// what the affected Chutes qwen models emit (`<think>…</think>\n\n<answer>`), and
/// it avoids the false positives a mid-string scan would invite (a model legitimately
/// discussing `<think>` tags in its answer).
fn split_leading_think_block(s: &str) -> Option<(String, String)> {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    // The block must be at the very start, modulo leading whitespace.
    let after_ws = s.trim_start();
    let inner_start = after_ws.strip_prefix(OPEN)?;
    // Require a matching close tag; an unclosed tag means we can't know where the
    // reasoning ends, so we leave the content untouched (never lose content).
    let close_idx = inner_start.find(CLOSE)?;
    let reasoning = inner_start[..close_idx].to_string();
    let remaining = &inner_start[close_idx + CLOSE.len()..];
    // Drop the whitespace that separated the think block from the actual answer.
    let answer = remaining.trim_start().to_string();
    Some((reasoning, answer))
}

/// Normalize the Chutes qwen reasoning shape on the NON-STREAM response (issue #779):
/// those models return chain-of-thought as a literal `<think>…</think>` block at the
/// start of `choices[].message.content` with `reasoning_content` absent/empty, where
/// minimax (Chutes) and GLM-5.1 (NEAR) correctly populate `reasoning_content`. For
/// each choice whose `message.content` is a string beginning with a well-formed
/// `<think>…</think>` block AND whose `reasoning_content` is absent/empty, we MOVE the
/// inner think text into `message.reasoning_content` and strip the block (and the
/// whitespace separating it from the answer) from `message.content`.
///
/// We never clobber a populated `reasoning_content` (so minimax is untouched), never
/// touch a `content` that isn't a leading-`<think>` string, and never lose content on
/// a malformed/unclosed tag (see [`split_leading_think_block`]). An *empty* block
/// (`<think></think>answer`) is stripped from `content` but does NOT set an empty
/// `reasoning_content` — that would contradict the "empty == unset" rule applied above.
///
/// `usage.completion_tokens_details.reasoning_tokens` is deliberately left untouched:
/// the upstream body carries no separate reasoning-token count and re-tokenizing here
/// to split `completion_tokens` would require the model tokenizer and risk a fabricated
/// number. Documented as a known limitation (tracked as a follow-up).
///
/// NON-STREAM ONLY: think tags span SSE chunks, so a correct stream parser must be
/// stateful across chunks — out of scope here (Chutes streaming is opt-in/experimental
/// anyway). The stream path's per-chunk [`transform_response_json`] does NOT run this.
fn normalize_think_reasoning(obj: &mut serde_json::Map<String, Value>) {
    let Some(choices) = obj.get_mut("choices").and_then(Value::as_array_mut) else {
        return;
    };
    for choice in choices {
        let Some(message) = choice.get_mut("message").and_then(Value::as_object_mut) else {
            continue;
        };
        // Don't clobber a reasoning field the provider already populated
        // (minimax / GLM-5.1). Absent OR empty/whitespace-only counts as "not set".
        let reasoning_already_set = message
            .get("reasoning_content")
            .and_then(Value::as_str)
            .is_some_and(|r| !r.trim().is_empty());
        if reasoning_already_set {
            continue;
        }
        // Only act on a plain-string content (the qwen shape). A content that is an
        // array of parts, null, or absent is left exactly as-is.
        let Some(content) = message.get("content").and_then(Value::as_str) else {
            continue;
        };
        let Some((reasoning, answer)) = split_leading_think_block(content) else {
            continue;
        };
        // Always strip the (well-formed) block from content. But only populate
        // `reasoning_content` when there is actual reasoning text — an empty or
        // whitespace-only block (e.g. `<think></think>answer`) would otherwise emit
        // a spurious `reasoning_content: ""`, contradicting the "empty == unset"
        // contract we apply to the provider's own field above.
        message.insert("content".to_string(), Value::String(answer));
        if reasoning.trim().is_empty() {
            message.remove("reasoning_content");
        } else {
            message.insert("reasoning_content".to_string(), Value::String(reasoning));
        }
    }
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// Length of the longest suffix of `buf` that is a proper, non-empty prefix of
/// `needle` (so it could be the start of a `needle` whose remainder hasn't arrived
/// yet). Used to hold back only the minimal ambiguous tail when a `<think>` /
/// `</think>` tag may be split across SSE chunks (e.g. `"<thi"` + `"nk>"`). Returns
/// `0` when no suffix of `buf` could begin `needle`. We only ever need to consider
/// suffixes shorter than `needle` (a full match is handled by the caller before
/// calling this), so the scan is bounded by `needle.len() - 1`.
fn ambiguous_tail_len(buf: &str, needle: &str) -> usize {
    let max = needle.len().saturating_sub(1).min(buf.len());
    // Longest first: the largest k where buf ends with needle[..k] (and needle
    // starts with that same slice, trivially). Must land on a char boundary so the
    // held-back tail is valid UTF-8.
    for k in (1..=max).rev() {
        if !needle.is_char_boundary(k) {
            continue;
        }
        let prefix = &needle[..k];
        if buf.len() >= k && buf.is_char_boundary(buf.len() - k) && buf.ends_with(prefix) {
            return k;
        }
    }
    0
}

/// Phase of the streaming `<think>…</think>` → `reasoning_content` extraction
/// (issue #779 stream follow-up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkPhase {
    /// Haven't yet decided whether `content` opens with a `<think>` block.
    Scanning,
    /// Inside a confirmed `<think>` block; inner text is routed to `reasoning_content`.
    InThink,
    /// Transform disabled for the rest of the stream — either we confirmed there is no
    /// leading `<think>` block, we saw the closing `</think>`, or the model emits native
    /// `reasoning_content`/`reasoning` (minimax-style). Content passes through verbatim.
    Done,
}

/// Stateful, low-latency extraction of a leading `<think>…</think>` block from the
/// streamed `content` deltas of a Chutes qwen response, mirroring the non-stream
/// [`normalize_think_reasoning`] across SSE chunk boundaries (issue #779 follow-up).
///
/// The streamed equivalent is genuinely harder than the non-stream one because the
/// `<think>`/`</think>` tags — and the reasoning text between them — can be split
/// across delta boundaries (`"<thi"` + `"nk>"`). We therefore parse incrementally and
/// hold back only the **minimal ambiguous suffix** (the longest tail that could be the
/// start of a tag we're scanning for), so a model that streams token-by-token still
/// streams with near-zero added latency — we never buffer the whole response.
///
/// Contract (matches the issue's hard requirements):
/// - **No-think model is byte-for-byte unchanged.** As soon as the first non-whitespace
///   `content` does not begin a `<think>` tag, we latch to [`ThinkPhase::Done`] and
///   re-emit everything held back, verbatim, as `content`.
/// - **Native-reasoning model untouched.** If any delta carries a non-empty
///   `reasoning_content` / `reasoning` while still scanning, we latch to `Done` and
///   never move anything (so minimax / GLM-5.1 are not clobbered).
/// - **Never lose content.** Held-back bytes are flushed at end-of-stream
///   ([`Self::flush`]): a never-resolved leading tail is flushed as `content`; an
///   unclosed `<think>` flushes its tail as `reasoning_content` (the text was inside the
///   block — it stays visible to the client, just under reasoning).
/// - **Single-choice only.** Multi-choice streaming (n>1) or a non-zero choice index
///   disables the transform (latch to `Done`) — the leading-block heuristic is only
///   well-defined for a single linear content stream.
///
/// The transform mutates the chunk JSON in place (it operates on the same parsed
/// `Value` that [`rewrite_sse_event_model`] sanitizes), rewriting `choices[0].delta`'s
/// `content` and inserting `reasoning_content`. It runs BEFORE the allowlist strip /
/// usage gate so its output is sanitized like any other field.
///
/// NOTE: Chutes streaming is opt-in/experimental (`CHUTES_ENABLE_STREAMING`); the inner
/// frames are not yet order-authenticated (see [`e2ee_stream`]).
#[derive(Debug)]
struct ThinkStreamState {
    phase: ThinkPhase,
    /// Minimal held-back suffix awaiting disambiguation. In `Scanning` it's the
    /// (possibly whitespace-prefixed) start that might open `<think>`; in `InThink`
    /// it's a possible partial `</think>`.
    pending: String,
}

impl ThinkStreamState {
    fn new() -> Self {
        Self {
            phase: ThinkPhase::Scanning,
            pending: String::new(),
        }
    }

    /// Apply the extraction to one decrypted chunk's JSON, in place. Mutates
    /// `choices[0].delta.content` (replacing it with the de-thought text, possibly
    /// empty) and inserts `choices[0].delta.reasoning_content` with the extracted
    /// reasoning when in/leaving a `<think>` block. Leaves the chunk untouched once
    /// in [`ThinkPhase::Done`].
    fn apply(&mut self, v: &mut Value) {
        if self.phase == ThinkPhase::Done {
            return;
        }
        let Some(choices) = v.get_mut("choices").and_then(Value::as_array_mut) else {
            return;
        };
        // The leading-`<think>` heuristic is only well-defined for a single linear
        // content stream. Anything else (n>1, or an unexpected non-zero index) disables
        // the transform for the rest of the stream rather than guessing.
        if choices.len() > 1 {
            self.phase = ThinkPhase::Done;
            return;
        }
        let Some(choice) = choices.first_mut() else {
            return;
        };
        if choice.get("index").and_then(Value::as_i64).unwrap_or(0) != 0 {
            self.phase = ThinkPhase::Done;
            return;
        }
        let Some(delta) = choice.get_mut("delta").and_then(Value::as_object_mut) else {
            return; // role-only / finish-only / non-delta chunk: nothing to move.
        };

        // A native reasoning field (minimax / GLM-5.1) means the model already splits
        // reasoning out — never transform such a model. Latch off while still scanning.
        let native_reasoning = ["reasoning_content", "reasoning"].iter().any(|k| {
            delta
                .get(*k)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
        });
        if native_reasoning && self.phase == ThinkPhase::Scanning {
            self.phase = ThinkPhase::Done;
            // We may still be holding leading bytes from earlier scanning chunks (a
            // whitespace run or a partial-`<think>` prefix that was never confirmed).
            // Those are ordinary content — prepend them to THIS delta's content so they
            // are never lost (Done's `flush` is a no-op, so we can't defer them).
            if !self.pending.is_empty() {
                // Reuse the owned `held` allocation rather than building a fresh String:
                // append the delta's existing content onto the held prefix in place.
                let mut held = std::mem::take(&mut self.pending);
                let rest = delta
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                held.push_str(rest);
                delta.insert("content".to_string(), Value::String(held));
            }
            return;
        }

        // Only a plain-string content participates; absent/null/array content is left
        // exactly as-is (the chunk may still carry tool_calls, role, etc.).
        let Some(incoming) = delta.get("content").and_then(Value::as_str) else {
            return;
        };
        let incoming = incoming.to_string();

        let (content_out, reasoning_out) = self.feed(&incoming);

        // Rewrite content: an empty string is still a valid (held-back) delta; we keep
        // the key present so the client's reassembly sees a content delta of "".
        delta.insert("content".to_string(), Value::String(content_out));
        if let Some(r) = reasoning_out {
            // Merge with any reasoning text already on this delta (there won't normally
            // be one while we own the transform, but never clobber if there is).
            let merged = match delta.get("reasoning_content").and_then(Value::as_str) {
                Some(existing) => format!("{existing}{r}"),
                None => r,
            };
            delta.insert("reasoning_content".to_string(), Value::String(merged));
        }
    }

    /// Core incremental state machine: feed one delta's content text, return
    /// `(content_to_emit, reasoning_to_emit)`. Pure (no JSON), so it's unit-tested
    /// directly. `pending`/`phase` carry the cross-chunk state.
    fn feed(&mut self, incoming: &str) -> (String, Option<String>) {
        match self.phase {
            ThinkPhase::Done => (incoming.to_string(), None),
            ThinkPhase::Scanning => self.feed_scanning(incoming),
            ThinkPhase::InThink => self.feed_in_think(incoming),
        }
    }

    fn feed_scanning(&mut self, incoming: &str) -> (String, Option<String>) {
        self.pending.push_str(incoming);
        // Whitespace before the block is dropped (matching the non-stream version),
        // but only ONCE we know a block follows — so keep the raw `pending` around for
        // the no-block flush and decide on its trimmed view.
        let trimmed = self.pending.trim_start();

        if let Some(rest) = trimmed.strip_prefix(THINK_OPEN) {
            // Confirmed: a leading `<think>`. Drop the whitespace + open tag, switch to
            // InThink, and run the remainder through the in-think handler so a
            // `</think>` already present in this same chunk is honored immediately.
            let rest = rest.to_string();
            self.pending.clear();
            self.phase = ThinkPhase::InThink;
            return self.feed_in_think(&rest);
        }

        // Still ambiguous: the trimmed start could be the beginning of `<think>` that
        // hasn't fully arrived (e.g. "<thi"). Hold everything, emit nothing yet.
        if THINK_OPEN.starts_with(trimmed) {
            return (String::new(), None);
        }

        // Decided: NOT a think block. Emit everything held back verbatim (whitespace
        // included — byte-for-byte passthrough) and disable the transform.
        self.phase = ThinkPhase::Done;
        let out = std::mem::take(&mut self.pending);
        (out, None)
    }

    fn feed_in_think(&mut self, incoming: &str) -> (String, Option<String>) {
        self.pending.push_str(incoming);

        if let Some(close_idx) = self.pending.find(THINK_CLOSE) {
            // Close tag found: text before it is the tail of the reasoning; text after
            // (with the whitespace that separated the block from the answer trimmed,
            // matching the non-stream version) begins the visible content.
            let reasoning: String = self.pending[..close_idx].to_string();
            let after = self.pending[close_idx + THINK_CLOSE.len()..]
                .trim_start()
                .to_string();
            self.pending.clear();
            self.phase = ThinkPhase::Done;
            let reasoning = (!reasoning.is_empty()).then_some(reasoning);
            return (after, reasoning);
        }

        // No close yet: emit all but the minimal suffix that could be a partial
        // `</think>`, holding that suffix for the next chunk.
        let tail = ambiguous_tail_len(&self.pending, THINK_CLOSE);
        let split = self.pending.len() - tail;
        let reasoning = self.pending[..split].to_string();
        self.pending.drain(..split);
        let reasoning = (!reasoning.is_empty()).then_some(reasoning);
        (String::new(), reasoning)
    }

    /// Flush any held-back bytes at end-of-stream so no content is ever lost.
    /// Returns `(content, reasoning)` to emit in a trailing delta, or `None` if there
    /// is nothing pending. A never-resolved leading tail (we never confirmed a block)
    /// is flushed as `content`; an unclosed `<think>` flushes its tail as
    /// `reasoning_content` (it was inside the block).
    fn flush(&mut self) -> Option<(Option<String>, Option<String>)> {
        if self.pending.is_empty() {
            return None;
        }
        let pending = std::mem::take(&mut self.pending);
        match self.phase {
            // Scanning: the held tail was a leading-whitespace / partial-`<think>` that
            // never completed → it's ordinary content, emit verbatim.
            ThinkPhase::Scanning => {
                self.phase = ThinkPhase::Done;
                Some((Some(pending), None))
            }
            // InThink: an unclosed `<think>` (a partial `</think>` tail at EOF). The text
            // was inside the block → keep it as reasoning, don't fabricate content.
            ThinkPhase::InThink => {
                self.phase = ThinkPhase::Done;
                Some((None, Some(pending)))
            }
            ThinkPhase::Done => None,
        }
    }
}

/// Build a trailing SSE event carrying the flushed `(content, reasoning)` from
/// [`ThinkStreamState::flush`], cloning the framing (id/model/created/index) of the
/// last real chunk so the synthetic delta is a well-formed continuation. Returns
/// `None` if there's nothing to flush or no template chunk to clone from.
fn flush_event(
    content: Option<String>,
    reasoning: Option<String>,
    template: Option<&Value>,
) -> Option<SSEEvent> {
    let template = template?;
    let mut delta = serde_json::Map::new();
    if let Some(c) = content {
        delta.insert("content".to_string(), Value::String(c));
    }
    if let Some(r) = reasoning {
        delta.insert("reasoning_content".to_string(), Value::String(r));
    }
    if delta.is_empty() {
        return None;
    }
    let mut v = json!({
        "id": template.get("id").cloned().unwrap_or(Value::String(String::new())),
        "object": "chat.completion.chunk",
        "created": template.get("created").cloned().unwrap_or(json!(0)),
        "model": template.get("model").cloned().unwrap_or(Value::String(String::new())),
        "choices": [{ "index": 0, "delta": Value::Object(delta) }],
    });
    // Sanitize the synthetic chunk the same way real ones are (defense-in-depth; the
    // template only contributes id/model/created so there's nothing to strip, but keep
    // the shapes identical).
    if let Some(obj) = v.as_object_mut() {
        strip_internal_response_fields(obj);
    }
    let chunk = serde_json::from_value::<crate::ChatCompletionChunk>(v.clone())
        .ok()
        .map(StreamChunk::Chat);
    let json = serde_json::to_string(&v).ok()?;
    Some(SSEEvent {
        raw_bytes: bytes::Bytes::from(format!("data: {json}\n\n")),
        chunk,
        raw_passthrough: true,
    })
}

/// Parse the JSON value out of an SSE event's `raw_bytes` (`data: {…}`), returning
/// `None` for control lines ([DONE], blanks) or non-JSON-object payloads. Used so the
/// stateful think transform and the flush template share the same chunk view.
fn sse_event_json(ev: &SSEEvent) -> Option<Value> {
    ev.chunk.as_ref()?;
    let s = std::str::from_utf8(&ev.raw_bytes).ok()?;
    let content = s.strip_prefix("data:").map(str::trim).unwrap_or(s.trim());
    let v: Value = serde_json::from_str(content).ok()?;
    v.is_object().then_some(v)
}

/// Apply the stateful `<think>` extraction (issue #779 stream follow-up) to one
/// decrypted SSE event, in place: re-frame `raw_bytes` and re-derive `chunk` from the
/// think-transformed JSON. Control lines and non-JSON payloads pass through unchanged
/// (and don't advance the parser). On ANY parse/serialize failure the event is returned
/// untouched — we never drop or corrupt a chunk over the rewrite. Returns the (possibly
/// rewritten) event; the [`ThinkStreamState`] carries the cross-chunk state.
///
/// The rewrite is ATOMIC: `raw_bytes` is updated only once the parsed `chunk` re-derives
/// successfully, so `chunk` and `raw_bytes` never temporarily de-sync (mirrors
/// [`rewrite_sse_event_model`]). Runs BEFORE that helper so the moved/stripped reasoning
/// text is then allowlist-sanitized and usage-gated like any other field.
fn apply_think_to_event(mut ev: SSEEvent, think: &mut ThinkStreamState) -> SSEEvent {
    let Some(mut v) = sse_event_json(&ev) else {
        return ev;
    };
    think.apply(&mut v);
    let Ok(json) = serde_json::to_string(&v) else {
        return ev;
    };
    // Re-derive the parsed chunk from the transformed JSON so the chunk and raw_bytes
    // stay in lockstep (the re-serialized route paths read the chunk, not raw_bytes).
    // If the re-parse fails, leave the event entirely untouched rather than rewriting
    // raw_bytes alone (which would de-sync chunk vs raw_bytes).
    let Ok(c) = serde_json::from_str::<crate::ChatCompletionChunk>(&json) else {
        return ev;
    };
    ev.chunk = Some(StreamChunk::Chat(c));
    ev.raw_bytes = bytes::Bytes::from(format!("data: {json}\n\n"));
    ev
}

/// Wrap the decrypted Chutes SSE stream with the stateful `<think>` extraction
/// (issue #779 stream follow-up) followed by the per-chunk allowlist/usage/model
/// rewrite ([`rewrite_sse_event_model`]).
///
/// Stateful across chunks: a single [`ThinkStreamState`] threads the open/closed think
/// state through the whole stream. The transform holds back only the minimal ambiguous
/// suffix (a possibly-split tag), so streaming stays low-latency. At end-of-stream — the
/// authenticated inner `[DONE]` — any held-back bytes are flushed in a synthetic trailing
/// delta ([`ThinkStreamState::flush`] / [`flush_event`]) BEFORE `[DONE]`, so no content is
/// ever lost (a never-resolved leading tail → `content`; an unclosed `<think>` →
/// `reasoning_content`). The flush delta is itself put through `rewrite_sse_event_model`.
///
/// Errors propagate unchanged; on a transport/decrypt error the upstream stream surfaces
/// the error frame (a failed stream — held-back bytes there are moot since the client
/// already sees the failure).
fn stream_with_think_extraction(
    decoded: StreamingResult,
    canonical: Option<String>,
    client_wants_usage: bool,
) -> StreamingResult {
    let s = async_stream::try_stream! {
        let mut decoded = decoded;
        let mut think = ThinkStreamState::new();
        // The last real chunk JSON, used to frame a synthetic flush delta (id/model/…).
        let mut last_template: Option<Value> = None;

        while let Some(item) = decoded.next().await {
            let ev = item?;
            // At the clean terminus, flush any held-back bytes BEFORE [DONE] so nothing
            // is lost, then forward [DONE] and end.
            if ev.is_done_marker() {
                if let Some((content, reasoning)) = think.flush() {
                    if let Some(flushed) = flush_event(content, reasoning, last_template.as_ref()) {
                        yield rewrite_sse_event_model(flushed, canonical.as_deref(), client_wants_usage);
                    }
                }
                yield ev;
                return;
            }
            let ev = apply_think_to_event(ev, &mut think);
            // Remember a usable template for the flush delta (id/model/created).
            if let Some(v) = sse_event_json(&ev) {
                last_template = Some(v);
            }
            yield rewrite_sse_event_model(ev, canonical.as_deref(), client_wants_usage);
        }
    };
    Box::pin(s)
}

/// An OpenAI request body (as JSON) with `model` pinned, `stream` set, and all
/// internal/tracing/E2EE-marker keys stripped (never sent to the third party).
fn request_body(model: &str, params: &ChatCompletionParams, stream: bool) -> Result<Value, String> {
    let mut v = serde_json::to_value(params).map_err(|e| format!("serialize params: {e}"))?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("model".to_string(), json!(model));
        obj.insert("stream".to_string(), json!(stream));
        if stream {
            // Force usage onto the final stream chunk so streamed tokens are
            // billed and counted against org limits (the OpenAI-compatible
            // default omits it, and our SSE adapter drops Chutes' outer
            // usage-only events). Matches every other provider.
            //
            // Merge into any client-supplied `stream_options` (e.g.
            // `continuous_usage_stats`) rather than clobbering the whole object —
            // we only need to *guarantee* `include_usage`.
            match obj.get_mut("stream_options").and_then(Value::as_object_mut) {
                Some(existing) => {
                    existing.insert("include_usage".to_string(), json!(true));
                }
                None => {
                    obj.insert(
                        "stream_options".to_string(),
                        json!({ "include_usage": true }),
                    );
                }
            }
        }
        // Strip internal identifiers + client-E2EE markers so they never reach
        // Chutes inside the (encrypted) request body.
        for k in INTERNAL_KEYS {
            obj.remove(*k);
        }
    } else {
        return Err("chat params did not serialize to a JSON object".to_string());
    }
    Ok(v)
}

/// Reject a request that carries client-facing E2EE intent (the client asked
/// cloud-api to encrypt the response to its key). The attested Chutes path does
/// not implement that response encryption, so we **reject** rather than silently
/// downgrade to a response the client would believe is E2EE but isn't.
fn reject_client_e2ee(params: &ChatCompletionParams) -> Result<(), CompletionError> {
    use crate::attested::nearai::encryption_headers as eh;
    if params.extra.contains_key(eh::CLIENT_PUB_KEY) {
        return Err(CompletionError::CompletionError(
            "client-facing E2EE is not supported on the attested Chutes path (responses arrive \
             over Chutes' own ML-KEM channel); omit the client encryption headers"
                .to_string(),
        ));
    }
    Ok(())
}

const UNSUPPORTED: &str =
    "operation not supported by the attested Chutes provider (chat completions only)";

#[async_trait]
impl InferenceProvider for Provider {
    async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        // The model set is configured explicitly; advertise just this one, under the
        // CANONICAL id (never the upstream `-TEE` chute slug).
        Ok(ModelsResponse {
            object: "list".to_string(),
            data: vec![ModelInfo {
                created: 0,
                id: self.canonical_id.clone(),
                object: "model".to_string(),
                owned_by: "chutes".to_string(),
                context_length: None,
                max_model_len: None,
                top_provider: None,
            }],
        })
    }

    async fn chat_completion(
        &self,
        mut params: ChatCompletionParams,
        _request_hash: String,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        reject_client_e2ee(&params)?;
        // #666: Chutes serializes `ChatMessage.content` verbatim into the (E2EE)
        // request body, so drop Anthropic prompt-caching breakpoints before
        // `request_body` — the upstream may 400 on an unknown `cache_control`
        // content-part field (it only matters on the Anthropic upstream).
        crate::strip_cache_control(&mut params.messages);
        let body = request_body(&self.model_name, &params, false)
            .map_err(CompletionError::CompletionError)?;
        let prep = self.verify_and_prepare(&body).await?;

        let resp_blob = self
            .client
            .invoke_nonstream(&InvokeRequest {
                chute_id: &prep.chute_id,
                instance_id: &prep.instance_id,
                nonce_token: &prep.nonce_token,
                path: CHAT_PATH,
                mode: InvokeMode::NonStream,
                blob: prep.blob,
            })
            .await
            .map_err(|e| Self::map_client_error("Chutes /e2e/invoke", e))?;

        let plaintext = prep
            .session
            .decrypt_response(&resp_blob)
            .map_err(|e| CompletionError::CompletionError(format!("E2EE decrypt: {e}")))?;

        // The upstream body carries the chute SLUG in `model`. When the canonical id
        // differs (NEAR-served / OpenRouter-id case), rewrite `model` to the
        // canonical id so the slug never leaks to clients and `response.model`
        // matches an id listable in /v1/models. In BOTH cases we also strip the
        // Chutes-internal/serving fields (#780: `prompt_sha256` — a user-prompt
        // fingerprint — plus `template_sha256`/`metadata`/`choices[].matched_stop`)
        // so a Chutes response matches the clean shape returned for first-party /
        // Anthropic / OpenAI models. We transform the JSON *value* (not the typed
        // struct), removing ONLY those named keys — so `chutes_verification` (the
        // attestation receipt) and any unmodeled passthrough field (e.g.
        // `hidden_states`) survive. No signature is sacrificed — Chutes responses
        // aren't separately signed (`supports_chat_signatures == false`).
        let canonical =
            (self.canonical_id != self.model_name).then_some(self.canonical_id.as_str());
        let raw_bytes = transform_response_json(&plaintext, canonical).ok_or_else(|| {
            CompletionError::CompletionError("failed to sanitize Chutes response body".to_string())
        })?;
        // Normalize the per-model reasoning shape (#779): the Chutes qwen models leak
        // chain-of-thought as a literal `<think>…</think>` block in `message.content`
        // with `reasoning_content` null, while minimax/GLM-5.1 correctly populate
        // `reasoning_content`. Move the think text into `reasoning_content` and strip
        // it from `content`, without clobbering an already-populated `reasoning_content`
        // or ever losing content on a malformed tag. Runs ONLY on this non-stream path
        // (think tags span SSE chunks — the stream path is intentionally not normalized;
        // see `normalize_think_reasoning`). This is applied AFTER `transform_response_json`
        // so it composes with the canonical-id rewrite + #780 internal-field strip, and
        // is the byte payload the route uses for hashing/passthrough.
        let raw_bytes = {
            let mut v: Value = serde_json::from_slice(&raw_bytes).map_err(|e| {
                CompletionError::CompletionError(format!("parse sanitized response: {e}"))
            })?;
            if let Some(o) = v.as_object_mut() {
                normalize_think_reasoning(o);
            }
            serde_json::to_vec(&v).map_err(|e| {
                CompletionError::CompletionError(format!("reserialize response: {e}"))
            })?
        };
        let response: ChatCompletionResponse = serde_json::from_slice(&raw_bytes)
            .map_err(|e| CompletionError::CompletionError(format!("parse response: {e}")))?;
        Ok(ChatCompletionResponseWithBytes {
            response,
            raw_bytes,
            serving_tier: crate::ProviderTier::Attested3p,
        })
    }

    async fn chat_completion_stream(
        &self,
        mut params: ChatCompletionParams,
        _request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        // Streaming is off by default: Chutes' stream protocol has no
        // authenticated frame ordering (content frames carry no sequence numbers),
        // so an on-path gateway could drop, reorder, or replay AEAD-valid frames
        // undetectably. It isn't honestly attested until Chutes adds sequence
        // numbers or a transcript MAC. Opt in with CHUTES_ENABLE_STREAMING.
        // Non-streaming is attested.
        if !self.allow_streaming {
            return Err(CompletionError::CompletionError(
                "Chutes streaming is not enabled as an attested path (frame ordering is not \
                 cryptographically authenticated); use non-streaming, or set \
                 CHUTES_ENABLE_STREAMING to opt in"
                    .to_string(),
            ));
        }
        reject_client_e2ee(&params)?;
        // #666: drop Anthropic prompt-caching breakpoints before serialization
        // (see the non-streaming path for the rationale).
        crate::strip_cache_control(&mut params.messages);
        // Capture the CLIENT's usage intent before `request_body` force-sets
        // `include_usage: true` upstream (we always request usage so billing sees
        // it; #781 L1 gates what the client receives). A client opts in via
        // `stream_options.include_usage: true`, OR vLLM's `continuous_usage_stats`
        // (which also expects a usage-bearing final chunk).
        let client_wants_usage = params.stream_options.as_ref().is_some_and(|so| {
            so.include_usage == Some(true) || so.continuous_usage_stats == Some(true)
        });
        let body = request_body(&self.model_name, &params, true)
            .map_err(CompletionError::CompletionError)?;
        let prep = self.verify_and_prepare(&body).await?;

        let resp = self
            .client
            .invoke_stream(&InvokeRequest {
                chute_id: &prep.chute_id,
                instance_id: &prep.instance_id,
                nonce_token: &prep.nonce_token,
                path: CHAT_PATH,
                mode: InvokeMode::Stream,
                blob: prep.blob,
            })
            .await
            .map_err(|e| Self::map_client_error("Chutes /e2e/invoke (stream)", e))?;

        // Decrypt the E2EE SSE into OpenAI SSEEvents (transport errors → CompletionError).
        let byte_stream = resp.bytes_stream().map(|r| {
            r.map_err(|e| CompletionError::CompletionError(format!("Chutes stream transport: {e}")))
        });
        let decoded = e2ee_stream::decrypt_e2ee_sse(Box::pin(byte_stream), prep.session);
        // Per-chunk transform, mirroring the non-stream path:
        //  1. stateful `<think>…</think>` → `reasoning_content` extraction (issue #779
        //     stream follow-up): route a leading chain-of-thought block to streamed
        //     `reasoning_content` deltas and keep it out of `content`, incrementally
        //     across SSE chunk boundaries (no whole-stream buffering). No-think and
        //     native-reasoning models stream through unchanged — see [`ThinkStreamState`];
        //  2. sanitize each decrypted `data:` line to the standard OpenAI allowlist
        //     (#780 → #781), gate the streamed `usage` field to the spec (#781 L1), and —
        //     only when the canonical id differs from the chute slug — rewrite `model` so
        //     streamed chunks never leak the slug. We ALWAYS run the sanitize/gate (even
        //     when canonical == slug), since serving internals and per-chunk `usage` can
        //     appear on the stream regardless of whether a model rewrite is needed.
        let canonical = (self.canonical_id != self.model_name).then(|| self.canonical_id.clone());
        Ok(stream_with_think_extraction(
            decoded,
            canonical,
            client_wants_usage,
        ))
    }

    async fn text_completion_stream(
        &self,
        _params: CompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        Err(CompletionError::CompletionError(UNSUPPORTED.to_string()))
    }

    async fn image_generation(
        &self,
        _params: ImageGenerationParams,
        _request_hash: String,
    ) -> Result<ImageGenerationResponseWithBytes, ImageGenerationError> {
        Err(ImageGenerationError::GenerationError(
            UNSUPPORTED.to_string(),
        ))
    }

    async fn image_edit(
        &self,
        _params: Arc<ImageEditParams>,
        _request_hash: String,
    ) -> Result<ImageEditResponseWithBytes, ImageEditError> {
        Err(ImageEditError::EditError(UNSUPPORTED.to_string()))
    }

    async fn audio_transcription(
        &self,
        _params: AudioTranscriptionParams,
        _request_hash: String,
    ) -> Result<AudioTranscriptionResponse, AudioTranscriptionError> {
        Err(AudioTranscriptionError::TranscriptionError(
            UNSUPPORTED.to_string(),
        ))
    }

    async fn score(
        &self,
        _params: ScoreParams,
        _request_hash: String,
    ) -> Result<ScoreResponse, ScoreError> {
        Err(ScoreError::GenerationError(UNSUPPORTED.to_string()))
    }

    async fn rerank(&self, _params: RerankParams) -> Result<RerankResponse, RerankError> {
        Err(RerankError::GenerationError(UNSUPPORTED.to_string()))
    }

    async fn embeddings_raw(
        &self,
        _body: bytes::Bytes,
        _extra: HashMap<String, Value>,
    ) -> Result<bytes::Bytes, EmbeddingError> {
        Err(EmbeddingError::RequestFailed(UNSUPPORTED.to_string()))
    }

    async fn privacy_classify_raw(
        &self,
        _body: bytes::Bytes,
        _extra: HashMap<String, Value>,
    ) -> Result<bytes::Bytes, PrivacyClassifyError> {
        Err(PrivacyClassifyError::RequestFailed(UNSUPPORTED.to_string()))
    }

    async fn get_attestation_report(
        &self,
        _model: String,
        _signing_algo: Option<String>,
        nonce: Option<String>,
        _signing_address: Option<String>,
        _include_tls_fingerprint: bool,
    ) -> Result<serde_json::Map<String, serde_json::Value>, AttestationError> {
        // Same memoized chute_id + candidate-iteration as the data path
        // (`verify_and_prepare`), so the report can't verify a different instance
        // than the one that would actually serve, and a single bad instance
        // doesn't fail the whole report.
        let chute_id = self
            .cached_chute_id()
            .await
            .map_err(|e| AttestationError::FetchError(format!("resolve chute_id: {e}")))?;
        let instances = self
            .client
            .discover_instances(&chute_id)
            .await
            .map_err(|e| AttestationError::FetchError(format!("discover instances: {e}")))?;
        let candidates: Vec<&client::E2eInstance> = instances
            .instances
            .iter()
            .filter(|i| !i.e2e_pubkey.is_empty())
            .collect();
        if candidates.is_empty() {
            return Err(AttestationError::FetchError(
                "no E2E-capable Chutes instance".to_string(),
            ));
        }

        // Honor the caller's nonce only if it's a **bare** 32-byte hex value (no
        // `0x`) — one consistent policy with `ChutesReportDataVerifier`, which
        // rejects a `0x` prefix (the nonce is hashed verbatim into the binding).
        // Anything else (prefixed, wrong length, non-hex) → mint a fresh one.
        let boot_nonce = match nonce.as_deref() {
            Some(n) if hex::decode(n).map(|b| b.len() == 32).unwrap_or(false) => {
                n.to_ascii_lowercase()
            }
            _ => Self::random_boot_nonce().map_err(AttestationError::FetchError)?,
        };

        let evidence_resp = self
            .client
            .fetch_evidence(&chute_id, &boot_nonce)
            .await
            .map_err(|e| AttestationError::FetchError(format!("fetch evidence: {e}")))?;

        // Try each candidate until one verifies (mirrors verify_and_prepare).
        let mut last_err = String::from("no candidate instances");
        for inst in candidates {
            let evidence = match evidence_resp.instance(&inst.instance_id) {
                Some(e) => e,
                None => {
                    last_err = format!("instance {} not present in /evidence", inst.instance_id);
                    continue;
                }
            };
            // Trim to match the data path's canonicalization (verify_and_prepare).
            let e2e_pubkey = inst.e2e_pubkey.trim();
            let info = match self
                .verifier
                .attest_instance(evidence, &boot_nonce, e2e_pubkey)
                .await
            {
                Ok(info) => info,
                Err(e) => {
                    last_err = format!("instance {} attestation failed: {e}", inst.instance_id);
                    continue;
                }
            };

            // A self-describing, independently re-verifiable report: the verdict
            // plus the raw quote + cert so a client can recompute the bindings.
            let mut m = serde_json::Map::new();
            m.insert("provider".to_string(), json!("chutes"));
            m.insert("verified".to_string(), json!(true));
            // Client-visible surface: report the canonical id, never the chute slug.
            m.insert("model".to_string(), json!(self.canonical_id));
            m.insert("instance_id".to_string(), json!(info.instance_id));
            m.insert(
                "measurement_config".to_string(),
                json!(info.measurement_config),
            );
            m.insert("tcb_status".to_string(), json!(info.tcb_status));
            m.insert("gpu_verdict".to_string(), json!(info.gpu_verdict));
            m.insert("e2e_pubkey".to_string(), json!(info.e2e_pubkey));
            m.insert("nonce".to_string(), json!(boot_nonce));
            m.insert("quote_b64".to_string(), json!(evidence.quote));
            m.insert("certificate_b64".to_string(), json!(evidence.certificate));
            return Ok(m);
        }
        Err(AttestationError::FetchError(format!(
            "all candidate Chutes instances failed attestation; last: {last_err}"
        )))
    }

    /// Chutes' response integrity is the ML-KEM E2EE channel's AEAD tag, not a
    /// per-response signature — so the attestation flow skips the signature
    /// fetch/store step entirely (rather than calling `get_signature` and
    /// erroring on every completion).
    ///
    /// CLIENT-VISIBLE TRADE-OFF (#758): under one canonical id with tiered fallback,
    /// a NEAR-served response is signature-available but a response that fell back to
    /// Chutes is not — so per-request signature availability is non-deterministic for
    /// such a model. This should be documented for clients of any model that lists
    /// both a NEAR and a Chutes provider.
    fn supports_chat_signatures(&self) -> bool {
        false
    }

    /// Attested third party: a NEAR-served model prefers its own fleet and only
    /// falls back to Chutes when the NEAR backends can't fulfill the request;
    /// a Chutes-only model has no NEAR tier so this provider is primary.
    fn tier(&self) -> crate::ProviderTier {
        crate::ProviderTier::Attested3p
    }

    fn provider_source(&self) -> crate::ProviderSource {
        crate::ProviderSource::Chutes
    }

    /// Chutes only serves an attested STREAM when streaming is explicitly enabled
    /// (its stream protocol has no authenticated frame ordering, so it's gated).
    /// Reporting this lets the pool route a streaming request to a NEAR sibling
    /// instead of falling through to a hard streaming-disabled error.
    fn supports_streaming(&self) -> bool {
        self.allow_streaming
    }

    /// Chutes can't serve client-facing E2EE (responses ride its own ML-KEM
    /// channel; `reject_client_e2ee` refuses the `x_client_pub_key` headers). The
    /// pool uses this to keep such requests on a NEAR sibling rather than falling
    /// through to that hard rejection.
    fn supports_client_e2ee(&self) -> bool {
        false
    }

    async fn get_signature(
        &self,
        _chat_id: &str,
        _signing_algo: Option<String>,
    ) -> Result<ChatSignature, CompletionError> {
        // Not reached via the normal flow (see supports_chat_signatures = false);
        // kept explicit so a direct caller gets a clear, non-panicking answer.
        Err(CompletionError::CompletionError(
            "Chutes provides E2EE-channel (AEAD) integrity, not a separate response signature"
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attested::chutes::evidence::InstanceEvidence;
    use crate::attested::chutes::verifier_port::VerifiedInstanceInfo;

    /// Issue #774: a Chutes upstream HTTP status (notably the `/e2e/instances`
    /// rate-limit 429) must surface as a RETRYABLE `HttpError { status_code }`, not
    /// a flat `CompletionError(String)` that the classifier masks as a
    /// non-retryable 502. Pins the status-preservation seam (`map_client_error`).
    #[test]
    fn map_client_error_preserves_http_status_for_retryability() {
        use client::ChutesClientError;

        // 429 from discovery → retryable HttpError{429} (→ RateLimitExceeded).
        match Provider::map_client_error(
            "discover instances",
            ChutesClientError::Status {
                status: 429,
                body: "rate limit exceeded".into(),
            },
        ) {
            CompletionError::HttpError {
                status_code,
                is_external,
                message,
            } => {
                assert_eq!(status_code, 429);
                assert!(is_external, "Chutes is an external upstream");
                assert!(
                    message.contains("discover instances") && message.contains("429"),
                    "message keeps stage + status for logs: {message}"
                );
            }
            other => panic!("429 must map to HttpError, got {other:?}"),
        }

        // 408 (provider timeout → 504; pool tries the next provider in the round
        // but does not retry the round) and 5xx (503 → ServiceOverloaded, retried)
        // are preserved too.
        assert!(matches!(
            Provider::map_client_error(
                "Chutes /e2e/invoke",
                ChutesClientError::Status {
                    status: 408,
                    body: String::new()
                },
            ),
            CompletionError::HttpError {
                status_code: 408,
                ..
            }
        ));
        assert!(matches!(
            Provider::map_client_error(
                "fetch evidence",
                ChutesClientError::Status {
                    status: 503,
                    body: String::new()
                },
            ),
            CompletionError::HttpError {
                status_code: 503,
                ..
            }
        ));

        // Non-HTTP-status errors (transport / decode / model-not-found) stay a
        // generic CompletionError — no fake status code invented.
        assert!(matches!(
            Provider::map_client_error("x", ChutesClientError::ModelNotFound("m".into())),
            CompletionError::CompletionError(_)
        ));
    }

    /// A 4xx that is NOT 408/429 (e.g. 400/413/422) on an internally-constructed
    /// discovery/evidence/invoke request is NOT the client's fault, so it must be
    /// collapsed to a generic `CompletionError` (masked 502) rather than preserved
    /// as an `HttpError`. Otherwise `map_provider_error` would route it to its
    /// `InvalidParams` arms and echo the stage label, provider name, and raw
    /// upstream body back to the customer as a misattributed HTTP 400.
    #[test]
    fn map_client_error_masks_internal_4xx_to_avoid_client_blame_and_leak() {
        use client::ChutesClientError;

        for status in [400u16, 413, 422] {
            let mapped = Provider::map_client_error(
                "Chutes /e2e/invoke",
                ChutesClientError::Status {
                    status,
                    body: "malformed encrypted request payload: secret-internal-detail".into(),
                },
            );
            match mapped {
                CompletionError::CompletionError(_) => {}
                other => panic!("internal {status} must mask to CompletionError, got {other:?}"),
            }
        }
    }

    #[test]
    fn map_client_error_retries_stale_invoke_400() {
        use client::ChutesClientError;

        let mapped = Provider::map_client_error(
            "Chutes /e2e/invoke",
            ChutesClientError::Status {
                status: 400,
                body: "nonce token expired for selected instance".into(),
            },
        );

        match mapped {
            CompletionError::HttpError {
                status_code,
                message,
                is_external,
            } => {
                assert_eq!(status_code, 503);
                assert!(is_external, "Chutes is an external upstream");
                assert!(
                    message.contains("Chutes /e2e/invoke") && message.contains("stale"),
                    "message should identify the retryable stale-target case: {message}"
                );
            }
            other => panic!("stale invoke 400 must map to retryable HttpError, got {other:?}"),
        }
    }

    /// #774 / #778 follow-up: a preserved (retryable) upstream status must carry
    /// the STAGE + STATUS only in its message — NEVER the raw upstream body. A 5xx
    /// body that happened to contain media/image keywords would otherwise trip the
    /// pool's `is_client_media_fetch_error` substring scan and get misclassified as
    /// a client error. The masked (non-status) arm keeps its full message (no
    /// status carried, so that scan never applies to it).
    #[test]
    fn map_client_error_preserved_status_message_omits_upstream_body() {
        use client::ChutesClientError;
        // A 5xx body crafted to trip a naive keyword scan, plus a generic secret.
        let leaky = "failed to fetch image media from internal://secret-host";
        for status in [429u16, 500, 502, 503] {
            match Provider::map_client_error(
                "discover instances",
                ChutesClientError::Status {
                    status,
                    body: leaky.into(),
                },
            ) {
                CompletionError::HttpError { message, .. } => {
                    assert!(
                        !message.contains("image") && !message.contains("media"),
                        "preserved-status message must not echo the upstream body: {message}"
                    );
                    assert!(
                        message.contains("discover instances")
                            && message.contains(&status.to_string()),
                        "message keeps stage + status: {message}"
                    );
                }
                other => panic!("status {status} must map to HttpError, got {other:?}"),
            }
        }
    }

    /// Build a `CachedInstances` from a list of (instance_id, nonces) pairs with a
    /// far-future expiry (so freshness checks treat it as live).
    fn cached(insts: &[(&str, &[&str])]) -> CachedInstances {
        CachedInstances {
            instances: insts
                .iter()
                .map(|(id, nonces)| client::E2eInstance {
                    instance_id: (*id).to_string(),
                    e2e_pubkey: "cGs=".to_string(), // non-empty (base64 "pk")
                    nonces: nonces.iter().map(|n| (*n).to_string()).collect(),
                })
                .collect(),
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        }
    }

    /// A single-use nonce is consumed exactly once: popping drains the pool and a
    /// given token is never returned twice. This is the reuse-prevention invariant
    /// `take_nonce` enforces at the cache level (the pool is a `Vec` we `.pop()`).
    #[test]
    fn cached_instances_nonce_consumed_at_most_once() {
        let mut c = cached(&[("i1", &["t-a", "t-b", "t-c"])]);
        let inst = c
            .instances
            .iter_mut()
            .find(|i| i.instance_id == "i1")
            .unwrap();
        let mut seen = std::collections::HashSet::new();
        let mut handed_out = Vec::new();
        while let Some(tok) = inst.nonces.pop() {
            assert!(seen.insert(tok.clone()), "token {tok} handed out twice");
            handed_out.push(tok);
        }
        assert_eq!(handed_out.len(), 3, "every token consumed exactly once");
        // Pool drained → no more tokens (the caller would move to the next candidate).
        assert!(inst.nonces.pop().is_none(), "drained pool yields None");
    }

    /// A missing instance id yields no token (mirrors `take_nonce` returning `None`
    /// → the caller skips to the next candidate).
    #[test]
    fn cached_instances_missing_instance_yields_none() {
        let c = cached(&[("i1", &["t-a"])]);
        assert!(
            !c.instances.iter().any(|i| i.instance_id == "nope"),
            "unknown instance id is absent → take_nonce returns None"
        );
    }

    /// The empty/expired seed (`CachedInstances::empty_expired`) must read as
    /// expired and carry no usable instance, so the FIRST `discover_cached`
    /// triggers a refresh. Pins the refresh-on-first-use contract without network.
    #[test]
    fn empty_expired_entry_signals_refresh_needed() {
        let c = CachedInstances::empty_expired();
        assert!(
            c.expires_at <= std::time::Instant::now(),
            "seed entry is already expired"
        );
        assert!(c.instances.is_empty(), "seed entry has no instances");
        // The discover_cached refresh predicate: expired OR no usable instance.
        let usable = c
            .instances
            .iter()
            .any(|i| !i.e2e_pubkey.is_empty() && !i.nonces.is_empty());
        assert!(
            !usable,
            "seed entry has no usable instance → refresh needed"
        );
    }

    /// A live snapshot whose instances all have EMPTY nonce pools is treated as
    /// "no usable instance" even though it hasn't expired — so `discover_cached`
    /// refreshes rather than handing out a chute with nothing to consume.
    #[test]
    fn fresh_but_drained_snapshot_signals_refresh_needed() {
        let c = cached(&[("i1", &[]), ("i2", &[])]); // not expired, but no nonces
        assert!(
            c.expires_at > std::time::Instant::now(),
            "snapshot is still within TTL"
        );
        let usable = c
            .instances
            .iter()
            .any(|i| !i.e2e_pubkey.is_empty() && !i.nonces.is_empty());
        assert!(
            !usable,
            "all-drained snapshot is unusable → discover_cached refreshes"
        );
    }

    /// `chute_cache` returns the SAME per-chute cell across calls (so refresh +
    /// nonce consumption serialize on one lock per chute), and DISTINCT cells for
    /// different chutes (so one chute's refresh never blocks another).
    #[tokio::test]
    async fn chute_cache_returns_stable_cell_per_chute() {
        let p = provider();
        let a1 = p.chute_cache("chute-A");
        let a2 = p.chute_cache("chute-A");
        let b1 = p.chute_cache("chute-B");
        assert!(Arc::ptr_eq(&a1, &a2), "same chute → same cell");
        assert!(!Arc::ptr_eq(&a1, &b1), "different chutes → distinct cells");
        // The cell really is the consumption point: seed it and consume via take_nonce.
        *a1.lock().await = cached(&[("i1", &["only-token"])]);
        assert_eq!(
            p.take_nonce("chute-A", "i1").await.as_deref(),
            Some("only-token"),
            "take_nonce consumes from the shared cell"
        );
        assert!(
            p.take_nonce("chute-A", "i1").await.is_none(),
            "second take on a 1-token pool drains it → None (no reuse)"
        );
        assert!(
            p.take_nonce("chute-A", "absent").await.is_none(),
            "missing instance → None"
        );
    }

    /// A verifier stub for unit tests (no DCAP/network): records calls and
    /// returns a fixed verdict or a forced error.
    struct StubVerifier {
        ok: bool,
    }

    #[async_trait]
    impl ChutesInstanceVerifier for StubVerifier {
        async fn attest_instance(
            &self,
            _evidence: &InstanceEvidence,
            _boot_nonce: &str,
            e2e_pubkey: &str,
        ) -> Result<VerifiedInstanceInfo, String> {
            if self.ok {
                Ok(VerifiedInstanceInfo {
                    instance_id: "i".to_string(),
                    e2e_pubkey: e2e_pubkey.to_string(),
                    measurement_config: "8xh200 v1.3.0".to_string(),
                    tcb_status: "UpToDate".to_string(),
                    gpu_verdict: "PASS".to_string(),
                })
            } else {
                Err("forced".to_string())
            }
        }
    }

    fn provider() -> Provider {
        Provider::new(
            Config::new(
                "cpk_test".to_string(),
                "zai-org/GLM-5.1-TEE".to_string(),
                30,
            ),
            Arc::new(StubVerifier { ok: true }),
        )
        .unwrap()
    }

    #[test]
    fn config_redacts_api_key() {
        let secret = "cpk_super_secret_value_0123456789";
        let s = format!("{:?}", Config::new(secret.into(), "m".into(), 60));
        assert!(s.contains("[redacted]"));
        assert!(!s.contains(secret));
    }

    #[test]
    fn config_non_positive_timeout_falls_back() {
        assert_eq!(
            Config::new("k".into(), "m".into(), -1).timeout_seconds(),
            DEFAULT_TIMEOUT_SECONDS
        );
        assert_eq!(
            Config::new("k".into(), "m".into(), 42).timeout_seconds(),
            42
        );
    }

    #[test]
    fn transform_response_json_rewrites_model_and_preserves_other_fields() {
        // The canonical-id rewrite that keeps the chute slug out of `response.model`
        // while preserving every other (incl. unmodeled) field.
        let body =
            br#"{"id":"x","model":"zai-org/GLM-5.1-TEE","choices":[],"hidden_states":[1,2]}"#;
        let out = transform_response_json(body, Some("zai-org/GLM-5.1-FP8")).expect("rewrite");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "zai-org/GLM-5.1-FP8");
        assert_eq!(v["id"], "x");
        // Unmodeled provider field survives the rewrite.
        assert_eq!(v["hidden_states"], json!([1, 2]));
        // Non-object body → None (left untouched by callers).
        assert!(transform_response_json(b"[1,2,3]", Some("x")).is_none());
        // With canonical == None, `model` is left as-is (strip-only mode).
        let out = transform_response_json(body, None).expect("strip-only");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "zai-org/GLM-5.1-TEE");
        assert_eq!(v["hidden_states"], json!([1, 2]));
    }

    #[test]
    fn transform_response_json_strips_internal_fields_keeps_verification_and_hidden_states() {
        // A Chutes-shaped non-stream body (#780): internal/serving fields present.
        let body = br#"{
            "id":"x",
            "model":"zai-org/GLM-5.1-TEE",
            "prompt_sha256":"deadbeef",
            "template_sha256":"cafef00d",
            "metadata":{"weight_version":"v3"},
            "chutes_verification":{"quote":"abc","instance_id":"i"},
            "hidden_states":[1,2,3],
            "choices":[
                {"index":0,"matched_stop":151643,"message":{"role":"assistant","content":"hi"}}
            ]
        }"#;
        // Strip-only mode (canonical == slug) and rewrite mode (canonical != slug)
        // must BOTH remove the internal fields.
        for canonical in [None, Some("zai-org/GLM-5.1-FP8")] {
            let out = transform_response_json(body, canonical).expect("transform");
            let v: Value = serde_json::from_slice(&out).unwrap();
            // The genuine privacy concern and the serving internals are gone.
            assert!(
                v.get("prompt_sha256").is_none(),
                "prompt_sha256 must be stripped"
            );
            assert!(
                v.get("template_sha256").is_none(),
                "template_sha256 must be stripped"
            );
            assert!(v.get("metadata").is_none(), "metadata must be stripped");
            // The attestation receipt is KEPT untouched.
            assert_eq!(v["chutes_verification"]["quote"], "abc");
            assert_eq!(v["chutes_verification"]["instance_id"], "i");
            // Unmodeled passthrough field survives.
            assert_eq!(v["hidden_states"], json!([1, 2, 3]));
            // Per-choice serving internal removed; other choice fields kept.
            assert!(
                v["choices"][0].get("matched_stop").is_none(),
                "choices[].matched_stop must be stripped"
            );
            assert_eq!(v["choices"][0]["index"], 0);
            assert_eq!(v["choices"][0]["message"]["content"], "hi");
        }
    }

    #[test]
    fn strip_internal_response_fields_handles_missing_choices() {
        // No `choices` array → only top-level keys removed, no panic. (The
        // non-object case is the caller's concern — `transform_response_json`
        // returns `None` and the body is kept verbatim; covered above.)
        let mut v = json!({"id":"x","prompt_sha256":"d","choices":"not-an-array"});
        let obj = v.as_object_mut().unwrap();
        strip_internal_response_fields(obj);
        assert!(obj.get("prompt_sha256").is_none());
        assert_eq!(obj["choices"], "not-an-array");
    }

    #[test]
    fn rewrite_sse_event_model_rewrites_data_chunk_but_passes_control_through() {
        use crate::{ChatCompletionChunk, StreamChunk};
        let chunk: ChatCompletionChunk = serde_json::from_value(json!({
            "id":"c","object":"chat.completion.chunk","created":0,
            "model":"zai-org/GLM-5.1-TEE","choices":[]
        }))
        .unwrap();
        let ev = SSEEvent {
            raw_bytes: bytes::Bytes::from_static(
                b"data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,\
                  \"model\":\"zai-org/GLM-5.1-TEE\",\"choices\":[]}\n\n",
            ),
            chunk: Some(StreamChunk::Chat(chunk)),
            raw_passthrough: true,
        };
        let out = rewrite_sse_event_model(ev, Some("zai-org/GLM-5.1-FP8"), false);
        let s = std::str::from_utf8(&out.raw_bytes).unwrap();
        assert!(
            s.contains("zai-org/GLM-5.1-FP8"),
            "data chunk model rewritten in raw_bytes"
        );
        assert!(
            !s.contains("GLM-5.1-TEE"),
            "slug must not leak in raw_bytes"
        );
        // The PARSED chunk's model must also be canonical — the route re-serializes
        // from `chunk` (not raw_bytes) on the auto-redact / alias-served paths.
        match &out.chunk {
            Some(StreamChunk::Chat(c)) => assert_eq!(c.model, "zai-org/GLM-5.1-FP8"),
            other => panic!("expected a rewritten Chat chunk, got {other:?}"),
        }

        // A control event (no chunk: [DONE]/blank) passes through unchanged.
        let ctrl = SSEEvent {
            raw_bytes: bytes::Bytes::from_static(b"data: [DONE]\n\n"),
            chunk: None,
            raw_passthrough: true,
        };
        let out = rewrite_sse_event_model(ctrl, Some("zai-org/GLM-5.1-FP8"), false);
        assert_eq!(&out.raw_bytes[..], b"data: [DONE]\n\n");
    }

    #[test]
    fn rewrite_sse_event_model_strips_internal_fields_per_chunk() {
        use crate::StreamChunk;
        // A final stream chunk carrying the Chutes-internal/serving fields (#780).
        // canonical == None (slug == canonical) is the strip-only mode that the
        // stream path uses for a Chutes-only model.
        // `matched_stop` appears BOTH at the choice level (no slot on `ChatChoice`,
        // dropped on parse) and nested in `delta` (captured by `ChatDelta::extra`) to
        // pin the defense-in-depth delta strip.
        let payload = r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"zai-org/GLM-5.1-TEE","prompt_sha256":"deadbeef","template_sha256":"cafef00d","metadata":{"weight_version":"v3"},"chutes_verification":{"quote":"abc"},"hidden_states":[7,8],"choices":[{"index":0,"matched_stop":151643,"delta":{"matched_stop":151643,"content":"hi"}}]}"#;
        // Build the parsed `chunk` from the SAME raw payload that production
        // deserializes (e2ee_stream), so the catch-all `extra` map actually
        // captures the internal fields — otherwise the chunk-side leak is invisible.
        let chunk: StreamChunk =
            StreamChunk::Chat(serde_json::from_str(payload).expect("parse chunk"));
        let ev = SSEEvent {
            raw_bytes: bytes::Bytes::from(format!("data: {payload}\n\n")),
            chunk: Some(chunk),
            raw_passthrough: true,
        };
        let out = rewrite_sse_event_model(ev, None, false);

        // raw_bytes: internal/serving fields stripped from the streamed line.
        let s = std::str::from_utf8(&out.raw_bytes).unwrap();
        let raw_payload = s.strip_prefix("data: ").unwrap().trim_end();
        let v: Value = serde_json::from_str(raw_payload).unwrap();
        assert!(v.get("prompt_sha256").is_none(), "prompt_sha256 stripped");
        assert!(
            v.get("template_sha256").is_none(),
            "template_sha256 stripped"
        );
        assert!(v.get("metadata").is_none(), "metadata stripped");
        assert!(
            v["choices"][0].get("matched_stop").is_none(),
            "choices[].matched_stop stripped"
        );
        assert!(
            v["choices"][0]["delta"].get("matched_stop").is_none(),
            "choices[].delta.matched_stop stripped (defense-in-depth)"
        );
        // The legitimate delta content survives the strip.
        assert_eq!(v["choices"][0]["delta"]["content"], "hi");
        // canonical == None → `model` (the slug here) is left as-is by this helper;
        // the strip happened regardless of any model rewrite.
        assert_eq!(v["model"], "zai-org/GLM-5.1-TEE");
        // Attestation receipt + unmodeled passthrough survive in raw_bytes.
        assert_eq!(v["chutes_verification"]["quote"], "abc");
        assert_eq!(v["hidden_states"], json!([7, 8]));

        // PARSED chunk: the route re-serializes from `chunk` (not raw_bytes) on the
        // auto-redact / alias-served paths, so the chunk MUST be sanitized too. Its
        // `extra` catch-all captured the internal top-level keys at parse time.
        let Some(StreamChunk::Chat(c)) = &out.chunk else {
            panic!("expected a Chat chunk");
        };
        let cv = serde_json::to_value(c).expect("serialize chunk");
        assert!(
            cv.get("prompt_sha256").is_none(),
            "prompt_sha256 must be stripped from the parsed chunk (extra)"
        );
        assert!(
            cv.get("template_sha256").is_none(),
            "template_sha256 must be stripped from the parsed chunk (extra)"
        );
        assert!(
            cv.get("metadata").is_none(),
            "metadata must be stripped from the parsed chunk (extra)"
        );
        // matched_stop has no slot on ChatChoice and is dropped on parse, so it's
        // already absent from the chunk regardless of the strip.
        assert!(
            cv["choices"][0].get("matched_stop").is_none(),
            "choices[].matched_stop absent from parsed chunk"
        );
        // The delta-nested matched_stop IS captured by ChatDelta::extra, so the
        // defense-in-depth strip must remove it from the parsed chunk too.
        assert!(
            cv["choices"][0]["delta"].get("matched_stop").is_none(),
            "choices[].delta.matched_stop must be stripped from the parsed chunk"
        );
        assert_eq!(cv["choices"][0]["delta"]["content"], "hi");
        // The attestation receipt + unmodeled passthrough survive on the chunk too
        // (both land in `extra` and are deliberately NOT stripped).
        assert_eq!(cv["chutes_verification"]["quote"], "abc");
        assert_eq!(cv["hidden_states"], json!([7, 8]));
    }

    #[test]
    fn transform_response_json_allowlist_drops_unknown_internals_keeps_standard() {
        // #781 allowlist: model-specific internals vary per model (kimi-k2.6 emits
        // `templated_prompt`/`prompt_token_ids`/`prompt_logprobs`/`kv_transfer_params`/
        // `choices[].token_ids`/`choices[].stop_reason`, etc.) so we drop by default.
        // Every standard OpenAI field — incl. `system_fingerprint`/`service_tier`,
        // which are STANDARD (not internals) — plus the kept extras
        // `chutes_verification`/`hidden_states` and the message-level
        // `reasoning_content`/`reasoning` MUST survive.
        let body = br#"{
            "id":"x","object":"chat.completion","created":1,"model":"moonshotai/Kimi-K2-TEE",
            "system_fingerprint":"fp_abc","service_tier":"default",
            "templated_prompt":"<rendered>","prompt_token_ids":[1,2,3],
            "prompt_logprobs":null,"kv_transfer_params":{"x":1},
            "prompt_sha256":"deadbeef","template_sha256":"cafef00d","metadata":{"w":"v3"},
            "chutes_verification":{"quote":"abc"},"hidden_states":[7,8],
            "choices":[{
                "index":0,"finish_reason":"stop","logprobs":null,
                "token_ids":[9,10],"stop_reason":151643,"matched_stop":151643,
                "message":{
                    "role":"assistant","content":"hi","reasoning_content":"because",
                    "reasoning":"because","refusal":null,"tool_calls":[],
                    "token_ids":[9,10],"stop_reason":151643
                }
            }],
            "usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}
        }"#;
        let out = transform_response_json(body, None).expect("transform");
        let v: Value = serde_json::from_slice(&out).unwrap();

        // Standard top-level fields survive.
        for k in [
            "id",
            "object",
            "created",
            "model",
            "choices",
            "usage",
            "system_fingerprint",
            "service_tier",
        ] {
            assert!(
                v.get(k).is_some(),
                "standard top-level field {k} must survive"
            );
        }
        // The deliberately-kept extras survive.
        assert_eq!(v["chutes_verification"]["quote"], "abc");
        assert_eq!(v["hidden_states"], json!([7, 8]));

        // Unknown / known internals dropped at the top level.
        for k in [
            "templated_prompt",
            "prompt_token_ids",
            "prompt_logprobs",
            "kv_transfer_params",
            "prompt_sha256",
            "template_sha256",
            "metadata",
        ] {
            assert!(
                v.get(k).is_none(),
                "internal top-level field {k} must be dropped"
            );
        }

        // Choice-level: standard kept, internals dropped.
        let choice = &v["choices"][0];
        assert_eq!(choice["index"], 0);
        assert_eq!(choice["finish_reason"], "stop");
        for k in ["token_ids", "stop_reason", "matched_stop"] {
            assert!(
                choice.get(k).is_none(),
                "choice-level internal {k} must be dropped"
            );
        }
        // Message-level: standard incl. both reasoning fields kept, internals dropped.
        let msg = &choice["message"];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "hi");
        assert_eq!(msg["reasoning_content"], "because");
        assert_eq!(msg["reasoning"], "because");
        for k in ["token_ids", "stop_reason"] {
            assert!(
                msg.get(k).is_none(),
                "message-level internal {k} must be dropped"
            );
        }
    }

    #[test]
    fn rewrite_sse_event_model_allowlist_drops_typed_internal_slots_on_chunk() {
        // The chunk side is re-derived from the sanitized raw_bytes, so even TYPED
        // internal slots on `ChatCompletionChunk` (`prompt_token_ids`, `modality`) and
        // `ChatChoice` (`token_ids`) — which a `chunk.extra`-only strip would miss —
        // are dropped from the re-serialized chunk. Standard fields + kept extras
        // survive on BOTH raw_bytes and the chunk.
        use crate::StreamChunk;
        let payload = r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"moonshotai/Kimi-K2-TEE","prompt_token_ids":[1,2],"modality":"text","prompt_sha256":"x","hidden_states":[7],"chutes_verification":{"q":"r"},"choices":[{"index":0,"token_ids":[9],"matched_stop":151643,"delta":{"content":"hi","reasoning_content":"why","token_ids":[9]}}]}"#;
        let chunk = StreamChunk::Chat(serde_json::from_str(payload).expect("parse chunk"));
        let ev = SSEEvent {
            raw_bytes: bytes::Bytes::from(format!("data: {payload}\n\n")),
            chunk: Some(chunk),
            raw_passthrough: true,
        };
        let out = rewrite_sse_event_model(ev, None, false);

        // raw_bytes: typed + extra internals dropped, standard + extras kept.
        let s = std::str::from_utf8(&out.raw_bytes).unwrap();
        let v: Value = serde_json::from_str(s.strip_prefix("data: ").unwrap().trim_end()).unwrap();
        for k in ["prompt_token_ids", "modality", "prompt_sha256"] {
            assert!(v.get(k).is_none(), "raw_bytes: {k} dropped");
        }
        assert_eq!(v["hidden_states"], json!([7]));
        assert_eq!(v["chutes_verification"]["q"], "r");
        assert!(v["choices"][0].get("token_ids").is_none());
        assert!(v["choices"][0].get("matched_stop").is_none());
        assert_eq!(v["choices"][0]["delta"]["content"], "hi");
        assert_eq!(v["choices"][0]["delta"]["reasoning_content"], "why");
        assert!(v["choices"][0]["delta"].get("token_ids").is_none());

        // Parsed chunk (re-serialized route paths): same allowlisted shape, incl. the
        // TYPED slots that a chunk.extra-only strip would have left behind.
        let Some(StreamChunk::Chat(c)) = &out.chunk else {
            panic!("expected a Chat chunk");
        };
        let cv = serde_json::to_value(c).expect("serialize chunk");
        assert!(
            cv.get("prompt_token_ids").is_none(),
            "typed prompt_token_ids dropped from re-derived chunk"
        );
        assert!(
            cv.get("modality").is_none(),
            "typed modality dropped from re-derived chunk"
        );
        assert!(
            cv["choices"][0].get("token_ids").is_none(),
            "typed choice token_ids dropped from re-derived chunk"
        );
        assert_eq!(cv["hidden_states"], json!([7]));
        assert_eq!(cv["chutes_verification"]["q"], "r");
        assert_eq!(cv["choices"][0]["delta"]["content"], "hi");
    }

    /// Build an SSE event from a raw chunk payload, run `rewrite_sse_event_model`,
    /// and return the client-facing `raw_bytes` parsed back to a JSON value.
    fn rewrite_raw(payload: &str, include_usage: bool) -> Value {
        use crate::StreamChunk;
        let chunk = StreamChunk::Chat(serde_json::from_str(payload).expect("parse chunk"));
        let ev = SSEEvent {
            raw_bytes: bytes::Bytes::from(format!("data: {payload}\n\n")),
            chunk: Some(chunk),
            raw_passthrough: true,
        };
        let out = rewrite_sse_event_model(ev, None, include_usage);
        let s = std::str::from_utf8(&out.raw_bytes).unwrap();
        serde_json::from_str(s.strip_prefix("data: ").unwrap().trim_end()).unwrap()
    }

    #[test]
    fn stream_usage_gate_strips_usage_from_intermediate_chunks() {
        // #781 L1: an intermediate content chunk (non-empty `choices`) must carry
        // `usage: null` regardless of include_usage — even when vLLM's
        // continuous_usage_stats populated it on every chunk.
        let intermediate = r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"hi"}}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#;
        for include_usage in [false, true] {
            let v = rewrite_raw(intermediate, include_usage);
            assert!(
                v.get("usage").is_none(),
                "intermediate chunk must drop usage (include_usage={include_usage})"
            );
            // Content is untouched by the usage gate.
            assert_eq!(v["choices"][0]["delta"]["content"], "hi");
        }
    }

    #[test]
    fn stream_usage_gate_final_chunk_honors_include_usage() {
        // The FINAL usage-only chunk has an empty `choices` array. When the client
        // requested include_usage it is forwarded with `usage` intact; otherwise the
        // WHOLE chunk is suppressed from the client stream — OpenAI emits no final
        // usage chunk at all in that case, and forwarding a gutted `choices: []`
        // husk breaks strict SDK parsers and reads as zero usage to cost-tracking
        // clients (BeeZi production feedback).
        use crate::StreamChunk;
        let final_chunk = r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":5,"total_tokens":8}}"#;

        // include_usage unset → the chunk is suppressed entirely (no husk).
        let chunk = StreamChunk::Chat(serde_json::from_str(final_chunk).expect("parse chunk"));
        let ev = SSEEvent {
            raw_bytes: bytes::Bytes::from(format!("data: {final_chunk}\n\n")),
            chunk: Some(chunk),
            raw_passthrough: true,
        };
        let out = rewrite_sse_event_model(ev, None, /* include_usage */ false);
        assert!(
            out.raw_bytes.is_empty(),
            "final usage chunk suppressed from client when include_usage unset"
        );

        // include_usage set → usage survives on the final chunk only.
        let v = rewrite_raw(final_chunk, true);
        assert_eq!(
            v["usage"]["total_tokens"], 8,
            "final chunk usage kept when include_usage set"
        );
    }

    #[test]
    fn stream_usage_gate_preserves_chunk_usage_for_billing() {
        // The client-facing raw_bytes honor the gate, but the parsed `chunk.usage`
        // (read by InterceptStream for billing) is preserved even when the client did
        // NOT request usage — billing must never be starved.
        use crate::StreamChunk;
        let final_chunk = r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":5,"total_tokens":8}}"#;
        let chunk = StreamChunk::Chat(serde_json::from_str(final_chunk).expect("parse chunk"));
        let ev = SSEEvent {
            raw_bytes: bytes::Bytes::from(format!("data: {final_chunk}\n\n")),
            chunk: Some(chunk),
            raw_passthrough: true,
        };
        let out = rewrite_sse_event_model(ev, None, /* include_usage */ false);

        // Client-facing raw_bytes: the whole final usage chunk is suppressed (OpenAI
        // sends no final chunk when include_usage wasn't requested — no husk).
        assert!(
            out.raw_bytes.is_empty(),
            "client raw_bytes suppressed for the gated final usage chunk"
        );

        // Parsed chunk: usage preserved for InterceptStream billing.
        let Some(StreamChunk::Chat(c)) = &out.chunk else {
            panic!("expected a Chat chunk");
        };
        let usage = c.usage.as_ref().expect("chunk.usage preserved for billing");
        assert_eq!(usage.total_tokens, 8);
    }

    #[test]
    fn transform_response_json_strips_unicode_escaped_key() {
        // A JSON key whose first char is unicode-escaped — "prompt_sha256" —
        // decodes to the plain field name `prompt_sha256`, so it must still be
        // stripped. The strip is by parsed key, not literal substring. This pins the
        // correctness reason we always round-trip rather than guarding with a cheap
        // substring scan, which such an escaped key would defeat. (Note: `\\u` here is
        // a single backslash in the Rust string, i.e. a real JSON `\u` escape.)
        let body = format!(
            r#"{{"id":"x","{}":"deadbeef","choices":[]}}"#,
            "\\u0070rompt_sha256"
        );
        // Sanity: the escaped key really does decode to the plain field name, and
        // the literal `prompt_sha256` substring is NOT present in the raw bytes.
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert!(
            parsed.get("prompt_sha256").is_some(),
            "escape decodes to key"
        );
        assert!(
            !body.contains("prompt_sha256"),
            "a literal-substring fast path would miss this key"
        );
        let out = transform_response_json(body.as_bytes(), None).expect("transform");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(
            v.get("prompt_sha256").is_none(),
            "unicode-escaped prompt_sha256 must be stripped"
        );
        assert_eq!(v["id"], "x");
    }

    /// Helper: build a non-stream chat-completion object, run the #779 think
    /// normalizer over it, and hand back the choice-0 message for assertions.
    fn normalize_msg(
        content: Value,
        reasoning_content: Option<Value>,
    ) -> serde_json::Map<String, Value> {
        let mut message = serde_json::Map::new();
        message.insert("role".to_string(), json!("assistant"));
        message.insert("content".to_string(), content);
        if let Some(rc) = reasoning_content {
            message.insert("reasoning_content".to_string(), rc);
        }
        let mut obj = json!({
            "id": "x",
            "object": "chat.completion",
            "model": "qwen/qwen3-32b",
            "choices": [{"index": 0, "finish_reason": "stop", "message": message}],
            "usage": {"prompt_tokens": 9, "completion_tokens": 285, "total_tokens": 294}
        })
        .as_object()
        .unwrap()
        .clone();
        normalize_think_reasoning(&mut obj);
        obj["choices"][0]["message"].as_object().unwrap().clone()
    }

    #[test]
    fn split_leading_think_block_extracts_and_strips() {
        // Well-formed block at the very start → reasoning + the trailing answer
        // (with the separating whitespace trimmed).
        let (r, a) =
            split_leading_think_block("<think>\nOkay, 2+2 is 4.\n</think>\n\n4").expect("split");
        assert_eq!(r, "\nOkay, 2+2 is 4.\n");
        assert_eq!(a, "4");
    }

    #[test]
    fn split_leading_think_block_tolerates_leading_whitespace_and_reasoning_only() {
        // Leading whitespace before <think> is fine.
        let (r, a) = split_leading_think_block("  <think>hmm</think>answer").expect("split");
        assert_eq!(r, "hmm");
        assert_eq!(a, "answer");
        // A reasoning-only response (nothing after </think>) → empty answer.
        let (r, a) = split_leading_think_block("<think>just thinking</think>").expect("split");
        assert_eq!(r, "just thinking");
        assert_eq!(a, "");
    }

    #[test]
    fn split_leading_think_block_returns_none_when_not_applicable() {
        // No tag at all.
        assert!(split_leading_think_block("just a plain answer").is_none());
        // Tag not at the start (answer first) — we don't touch mid-string tags.
        assert!(split_leading_think_block("the answer is <think>x</think>").is_none());
        // Unclosed / malformed — must NOT split (never lose content).
        assert!(split_leading_think_block("<think>unterminated reasoning...").is_none());
        // Empty string.
        assert!(split_leading_think_block("").is_none());
    }

    #[test]
    fn normalize_think_reasoning_moves_block_to_reasoning_content() {
        // The qwen #779 shape: <think>…</think> inside content, reasoning_content absent.
        let msg = normalize_msg(json!("<think>\nLet me work it out.\n</think>\n\n4"), None);
        assert_eq!(msg["content"], "4", "think block stripped from content");
        assert_eq!(
            msg["reasoning_content"], "\nLet me work it out.\n",
            "reasoning moved to reasoning_content"
        );
    }

    #[test]
    fn normalize_think_reasoning_does_not_clobber_existing_reasoning() {
        // minimax / GLM-5.1 shape: reasoning_content already populated, content clean.
        // Even if content somehow had a leading <think>, an already-set reasoning_content
        // means we leave the whole message untouched.
        let msg = normalize_msg(
            json!("<think>raw</think>final"),
            Some(json!("proper reasoning")),
        );
        assert_eq!(
            msg["reasoning_content"], "proper reasoning",
            "must not clobber a populated reasoning_content"
        );
        assert_eq!(
            msg["content"], "<think>raw</think>final",
            "content untouched when reasoning_content is already set"
        );
    }

    #[test]
    fn normalize_think_reasoning_treats_empty_reasoning_as_unset() {
        // An empty/whitespace-only reasoning_content counts as "not set", so we still
        // extract the leading think block into it.
        let msg = normalize_msg(json!("<think>think</think>answer"), Some(json!("   ")));
        assert_eq!(msg["content"], "answer");
        assert_eq!(msg["reasoning_content"], "think");
    }

    #[test]
    fn normalize_think_reasoning_leaves_content_without_think_intact() {
        // No think block → content and (absent) reasoning_content both untouched.
        let msg = normalize_msg(json!("just the answer, no reasoning"), None);
        assert_eq!(msg["content"], "just the answer, no reasoning");
        assert!(
            msg.get("reasoning_content").is_none(),
            "no reasoning_content fabricated"
        );
    }

    #[test]
    fn normalize_think_reasoning_empty_block_strips_without_empty_reasoning() {
        // An empty/whitespace-only `<think></think>` block: strip the tags from
        // content but do NOT emit an empty `reasoning_content` (would contradict the
        // "empty == unset" contract and surface a spurious field to clients).
        let msg = normalize_msg(json!("<think></think>answer"), None);
        assert_eq!(msg["content"], "answer", "empty think block stripped");
        assert!(
            msg.get("reasoning_content").is_none(),
            "no empty reasoning_content emitted"
        );
        // Whitespace-only block behaves the same.
        let msg = normalize_msg(json!("<think>   \n  </think>\n\nanswer"), None);
        assert_eq!(msg["content"], "answer");
        assert!(
            msg.get("reasoning_content").is_none(),
            "whitespace-only think block emits no reasoning_content"
        );
    }

    #[test]
    fn normalize_think_reasoning_leaves_malformed_think_intact() {
        // Unclosed <think> → never split (never lose content), no reasoning_content.
        let msg = normalize_msg(
            json!("<think>unterminated reasoning that never closes"),
            None,
        );
        assert_eq!(
            msg["content"], "<think>unterminated reasoning that never closes",
            "malformed/unclosed think must leave content intact"
        );
        assert!(
            msg.get("reasoning_content").is_none(),
            "no reasoning extracted from a malformed block"
        );
    }

    #[test]
    fn normalize_think_reasoning_ignores_non_string_content() {
        // A content that is an array of parts (multimodal) or null is left as-is —
        // the qwen leak is a plain-string shape only.
        let msg = normalize_msg(
            json!([{"type": "text", "text": "<think>x</think>hi"}]),
            None,
        );
        assert_eq!(
            msg["content"],
            json!([{"type": "text", "text": "<think>x</think>hi"}]),
            "array content untouched"
        );
        assert!(msg.get("reasoning_content").is_none());
    }

    #[test]
    fn normalize_think_reasoning_does_not_touch_usage() {
        // Documented limitation (#779): usage.completion_tokens (and any
        // reasoning_tokens) are left exactly as the upstream reported — we don't
        // fabricate a reasoning-token split.
        let mut obj = json!({
            "id": "x",
            "object": "chat.completion",
            "model": "qwen/qwen3-32b",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "<think>r</think>a"}}],
            "usage": {"prompt_tokens": 9, "completion_tokens": 285, "total_tokens": 294}
        })
        .as_object()
        .unwrap()
        .clone();
        normalize_think_reasoning(&mut obj);
        assert_eq!(obj["usage"]["completion_tokens"], 285, "usage untouched");
        assert_eq!(obj["usage"]["total_tokens"], 294);
        assert!(
            obj["usage"].get("reasoning_tokens").is_none()
                && obj["usage"].get("completion_tokens_details").is_none(),
            "no reasoning-token count fabricated"
        );
        // But the content/reasoning_content normalization still happened.
        assert_eq!(obj["choices"][0]["message"]["content"], "a");
        assert_eq!(obj["choices"][0]["message"]["reasoning_content"], "r");
    }

    #[test]
    fn rewrite_sse_event_model_leaves_chunk_bearing_event_untouched_on_rewrite_failure() {
        use crate::{ChatCompletionChunk, StreamChunk};
        // Pins the ATOMIC invariant: if the raw_bytes rewrite fails (here the
        // payload isn't a JSON object so transform_response_json returns None), the
        // event is returned fully unchanged — raw_bytes AND the parsed chunk both
        // keep their original value (model unchanged, no strip applied). Guards
        // against a future refactor reintroducing a half-rewritten state (chunk
        // mutated while raw_bytes still leaks). This failure path is unreachable in
        // practice (the decoder only sets chunk: Some when raw_bytes parsed as valid
        // JSON) but is cheap to pin and the only thing the atomicity reorder protects.
        let chunk: ChatCompletionChunk = serde_json::from_value(json!({
            "id":"c","object":"chat.completion.chunk","created":0,
            "model":"zai-org/GLM-5.1-TEE","choices":[]
        }))
        .unwrap();
        let ev = SSEEvent {
            // Non-object JSON → transform_response_json returns None → rewrite bails.
            raw_bytes: bytes::Bytes::from_static(b"data: [1,2,3]\n\n"),
            chunk: Some(StreamChunk::Chat(chunk)),
            raw_passthrough: true,
        };
        let out = rewrite_sse_event_model(ev, Some("zai-org/GLM-5.1-FP8"), false);
        // raw_bytes untouched (no canonical id introduced, no reframing).
        assert_eq!(&out.raw_bytes[..], b"data: [1,2,3]\n\n");
        // The parsed chunk's model must NOT have been mutated to canonical.
        match &out.chunk {
            Some(StreamChunk::Chat(c)) => assert_eq!(c.model, "zai-org/GLM-5.1-TEE"),
            other => panic!("expected the original Chat chunk, got {other:?}"),
        }
        assert!(out.raw_passthrough, "raw_passthrough preserved");
    }

    #[test]
    fn request_body_pins_model_and_stream() {
        let params: ChatCompletionParams = serde_json::from_value(json!({
            "model": "ignored", "messages": [{"role":"user","content":"hi"}]
        }))
        .unwrap();
        let body = request_body("zai-org/GLM-5.1-TEE", &params, true).unwrap();
        assert_eq!(body["model"], "zai-org/GLM-5.1-TEE");
        assert_eq!(body["stream"], true);
        // Streaming must request usage so tokens are billed/counted.
        assert_eq!(body["stream_options"]["include_usage"], true);

        // Non-streaming must NOT set stream_options.
        let body = request_body("m", &params, false).unwrap();
        assert_eq!(body["stream"], false);
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn request_body_merges_client_stream_options() {
        // A client-supplied stream_options (here arriving via `extra`, e.g.
        // continuous_usage_stats) must be preserved — we only force include_usage
        // for billing, we don't clobber the whole object.
        let mut params: ChatCompletionParams =
            serde_json::from_value(json!({"model": "m", "messages": []})).unwrap();
        params.extra.insert(
            "stream_options".to_string(),
            json!({"continuous_usage_stats": true}),
        );
        let body = request_body("m", &params, true).unwrap();
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["stream_options"]["continuous_usage_stats"], true);
    }

    #[test]
    fn request_body_strips_internal_and_e2ee_keys() {
        use crate::attested::nearai::{encryption_headers as eh, tracing_headers as th};
        let mut params: ChatCompletionParams =
            serde_json::from_value(json!({"model": "m", "messages": []})).unwrap();
        // Internal identifiers + client-E2EE markers must never reach Chutes.
        for k in [
            th::REQUEST_ID,
            th::ORG_ID,
            th::WORKSPACE_ID,
            eh::SIGNING_ALGO,
            eh::CLIENT_PUB_KEY,
            eh::MODEL_PUB_KEY,
            eh::ENCRYPTION_VERSION,
            eh::ENCRYPT_ALL_FIELDS,
        ] {
            params.extra.insert(k.to_string(), json!("leak"));
        }
        let body = request_body("m", &params, false).unwrap();
        let obj = body.as_object().unwrap();
        for k in INTERNAL_KEYS {
            assert!(
                !obj.contains_key(*k),
                "internal key {k} must not reach Chutes in the request body"
            );
        }
    }

    #[tokio::test]
    async fn rejects_client_facing_e2ee() {
        use crate::attested::nearai::encryption_headers as eh;
        let p = provider();
        let mut params: ChatCompletionParams =
            serde_json::from_value(json!({"model": "m", "messages": []})).unwrap();
        params
            .extra
            .insert(eh::CLIENT_PUB_KEY.to_string(), json!("clientkey"));
        // Rejected before any network (the check is first).
        match p.chat_completion(params, "h".into()).await {
            Err(CompletionError::CompletionError(msg)) => {
                assert!(msg.contains("client-facing E2EE"), "got: {msg}")
            }
            _ => panic!("expected client-E2EE rejection"),
        }
    }

    #[tokio::test]
    async fn provider_advertises_its_model() {
        let p = provider();
        let m = p.models().await.unwrap();
        assert_eq!(m.data.len(), 1);
        assert_eq!(m.data[0].id, "zai-org/GLM-5.1-TEE");
    }

    #[tokio::test]
    async fn streaming_gated_off_by_default() {
        // Default provider has allow_streaming=false; chat_completion_stream must
        // refuse before any network (the gate is the first check).
        let p = provider();
        // StreamingResult isn't Debug, so match rather than unwrap_err.
        match p
            .chat_completion_stream(
                serde_json::from_value(json!({"model": "m", "messages": []})).unwrap(),
                "h".into(),
            )
            .await
        {
            Err(CompletionError::CompletionError(msg)) => {
                assert!(msg.contains("streaming is not enabled"), "got: {msg}")
            }
            _ => panic!("expected streaming-disabled error"),
        }
    }

    #[tokio::test]
    async fn non_chat_modalities_unsupported() {
        // text_completion_stream returns the unsupported error with no network.
        let p = provider();
        assert!(p
            .text_completion_stream(
                serde_json::from_value(json!({"model":"m","prompt":"hi"})).unwrap()
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn get_signature_is_unsupported() {
        assert!(provider().get_signature("c", None).await.is_err());
    }

    /// LIVE probe (ignored) — the open question gating `CHUTES_ENABLE_STREAMING`:
    /// does Chutes terminate a stream with an **authenticated inner `[DONE]`**
    /// (decrypted from an `e2e` frame → our decoder yields it and ends cleanly) or
    /// only an **outer plaintext `[DONE]`** (which the decoder ignores as forgeable
    /// → EOF without an inner terminus → a fatal truncation error)?
    ///
    /// Uses the real E2EE path with a stub verifier (we're testing the stream
    /// protocol, not re-verifying attestation — the encaps pubkey still comes from
    /// the live discovered instance). The model defaults to `zai-org/GLM-5.1-TEE`
    /// but can be overridden via `CHUTES_PROBE_MODEL` so re-running the probe after
    /// that model is decommissioned needs no code edit. Run:
    ///   CHUTES_API_KEY=cpk_... cargo test -p inference_providers --lib \
    ///     attested::chutes::tests::live_chutes_streaming_done_probe -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "live Chutes streaming probe; needs CHUTES_API_KEY + network"]
    async fn live_chutes_streaming_done_probe() {
        use futures_util::StreamExt;
        let key = std::env::var("CHUTES_API_KEY").expect("set CHUTES_API_KEY for the live probe");
        let model =
            std::env::var("CHUTES_PROBE_MODEL").unwrap_or_else(|_| "zai-org/GLM-5.1-TEE".into());
        let provider = Provider::new(
            Config::new(key, model.clone(), 120).with_streaming(true),
            Arc::new(StubVerifier { ok: true }),
        )
        .unwrap();
        let params: ChatCompletionParams = serde_json::from_value(json!({
            "model": model,
            "messages": [{"role": "user", "content": "Count: 1 2 3, then stop."}],
            "max_tokens": 64,
            "temperature": 0,
            "stream": true
        }))
        .unwrap();
        let mut stream = provider
            .chat_completion_stream(params, "probe".to_string())
            .await
            .expect("stream should start");
        let mut events = 0usize;
        let mut inner_done = false;
        let mut err: Option<String> = None;
        let mut last = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(ev) => {
                    events += 1;
                    // The decoder filters the forgeable *outer* `[DONE]`
                    // (`handle_outer_payload` returns `Ok(None)`), so the only
                    // yielded done-marker is the authenticated inner one. Use the
                    // precise predicate rather than a raw-bytes substring scan,
                    // which model text containing "[DONE]" could false-positive.
                    if ev.is_done_marker() {
                        inner_done = true;
                    }
                    last = String::from_utf8_lossy(&ev.raw_bytes).to_string();
                }
                Err(e) => {
                    err = Some(format!("{e}"));
                    break;
                }
            }
        }
        eprintln!("PROBE: events={events} inner_done={inner_done} err={err:?}");
        eprintln!("PROBE last_event: {last}");
        assert!(
            err.is_none(),
            "stream errored before a clean terminus — Chutes likely sends only an \
             outer plaintext [DONE] (truncation): {err:?}"
        );
        assert!(
            inner_done,
            "stream ended without an inner [DONE] — streaming can't be honestly \
             attested until Chutes emits the terminator inside the channel"
        );
    }

    // ----------------------------------------------------------------------------
    // Stream `<think>…</think>` → reasoning_content extraction (issue #779 follow-up).
    // ----------------------------------------------------------------------------

    /// Build a stream data SSEEvent for a single-choice chunk with the given delta
    /// content, mirroring what `e2ee_stream::inner_event` produces.
    fn content_chunk_event(content: &str) -> SSEEvent {
        let payload = json!({
            "id": "c1",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "Qwen/Qwen3-TEE",
            "choices": [{ "index": 0, "delta": { "content": content } }],
        });
        let s = serde_json::to_string(&payload).unwrap();
        let chunk: crate::ChatCompletionChunk = serde_json::from_str(&s).unwrap();
        SSEEvent {
            raw_bytes: bytes::Bytes::from(format!("data: {s}\n\n")),
            chunk: Some(StreamChunk::Chat(chunk)),
            raw_passthrough: true,
        }
    }

    fn done_marker_event() -> SSEEvent {
        SSEEvent {
            raw_bytes: bytes::Bytes::from_static(b"data: [DONE]\n\n"),
            chunk: None,
            raw_passthrough: true,
        }
    }

    /// Run a list of content deltas (plus a final [DONE]) through the full stateful
    /// stream transform and collect, per emitted data chunk, the (content, reasoning)
    /// the client would see. Control lines are skipped. Mirrors production wiring.
    async fn run_think_stream(contents: &[&str]) -> Vec<(Option<String>, Option<String>)> {
        use futures_util::StreamExt;
        let mut events: Vec<Result<SSEEvent, CompletionError>> = contents
            .iter()
            .map(|c| Ok(content_chunk_event(c)))
            .collect();
        events.push(Ok(done_marker_event()));
        let decoded: StreamingResult = Box::pin(futures_util::stream::iter(events));
        let mut out = stream_with_think_extraction(decoded, None, false);
        let mut seen = Vec::new();
        while let Some(item) = out.next().await {
            let ev = item.unwrap();
            if ev.is_done_marker() {
                continue;
            }
            let s = std::str::from_utf8(&ev.raw_bytes).unwrap();
            let v: Value =
                serde_json::from_str(s.strip_prefix("data: ").unwrap().trim_end()).unwrap();
            let delta = &v["choices"][0]["delta"];
            seen.push((
                delta
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                delta
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ));
        }
        seen
    }

    /// Concatenate all content / reasoning deltas the client would receive.
    fn joined(seen: &[(Option<String>, Option<String>)]) -> (String, String) {
        let content: String = seen.iter().filter_map(|(c, _)| c.clone()).collect();
        let reasoning: String = seen.iter().filter_map(|(_, r)| r.clone()).collect();
        (content, reasoning)
    }

    #[test]
    fn ambiguous_tail_len_holds_partial_tags_only() {
        // Only a proper, non-empty prefix of the needle that ends `buf` is held back —
        // longest first. Full matches are the caller's job (handled before this is hit).
        // "</thi" (5 chars) is a prefix of "</think>", and buf ends with it → hold 5.
        assert_eq!(ambiguous_tail_len("hello</thi", THINK_CLOSE), 5);
        assert_eq!(ambiguous_tail_len("abc</", THINK_CLOSE), 2); // "</" is a prefix → hold 2
        assert_eq!(ambiguous_tail_len("plain text", THINK_CLOSE), 0); // no tag start → hold none
        assert_eq!(ambiguous_tail_len("ends with <", THINK_OPEN), 1); // "<" begins "<think>"
        assert_eq!(ambiguous_tail_len("no tag here", THINK_OPEN), 0);
        // A fully-present needle is capped at len-1 here (the longest *proper* prefix
        // that's also a suffix). "</think>" has no shorter prefix that is also a suffix,
        // so it returns 0 — the caller already handled the full match.
        assert_eq!(ambiguous_tail_len(THINK_CLOSE, THINK_CLOSE), 0);
    }

    #[tokio::test]
    async fn think_block_split_across_three_chunks() {
        // The reasoning is streamed in pieces and the tags straddle chunk boundaries:
        // "<thi"|"nk>reaso"|"ning</thi"|"nk>answer". Inner text must surface as
        // reasoning_content deltas and NOT as content; the answer must be clean content.
        let seen = run_think_stream(&["<thi", "nk>reaso", "ning</thi", "nk>he", "llo"]).await;
        let (content, reasoning) = joined(&seen);
        assert_eq!(
            reasoning, "reasoning",
            "inner think text → reasoning_content"
        );
        assert_eq!(content, "hello", "answer is clean content, no tags");
        assert!(
            !content.contains("<think>") && !content.contains("</think>"),
            "tags never leak into content"
        );
    }

    #[tokio::test]
    async fn think_block_in_single_chunk() {
        let seen = run_think_stream(&["<think>because</think>\n\nthe answer"]).await;
        let (content, reasoning) = joined(&seen);
        assert_eq!(reasoning, "because");
        assert_eq!(content, "the answer", "whitespace after </think> trimmed");
    }

    #[tokio::test]
    async fn no_think_model_passes_through_unchanged() {
        // A model that emits no think tags must pass through byte-for-byte: same content,
        // never any reasoning_content.
        let seen = run_think_stream(&["Hello", ", ", "world", "!"]).await;
        let (content, reasoning) = joined(&seen);
        assert_eq!(content, "Hello, world!");
        assert_eq!(
            reasoning, "",
            "no reasoning fabricated for a no-think model"
        );
    }

    #[tokio::test]
    async fn content_that_merely_mentions_think_is_not_treated_as_block() {
        // Answer text that talks about "<thinking>" (not the exact open tag) must NOT be
        // misread as a think block — the first non-ws chars decide.
        let seen = run_think_stream(&["I will <thinking> about it"]).await;
        let (content, reasoning) = joined(&seen);
        assert_eq!(content, "I will <thinking> about it");
        assert_eq!(reasoning, "");
    }

    #[tokio::test]
    async fn leading_text_then_think_is_passthrough() {
        // `<think>` not at the very start (answer first) → passthrough, matching the
        // non-stream `split_leading_think_block` contract.
        let seen = run_think_stream(&["answer first <think>x</think>"]).await;
        let (content, reasoning) = joined(&seen);
        assert_eq!(content, "answer first <think>x</think>");
        assert_eq!(reasoning, "");
    }

    #[tokio::test]
    async fn unclosed_think_never_loses_content() {
        // An unclosed `<think>` at EOF: the inner text was inside the block, so it stays
        // visible as reasoning_content (never dropped). No content is fabricated.
        let seen = run_think_stream(&["<think>thinking forever and ever"]).await;
        let (content, reasoning) = joined(&seen);
        assert_eq!(
            content, "",
            "no content for a reasoning-only (unclosed) stream"
        );
        assert_eq!(
            reasoning, "thinking forever and ever",
            "all inner text preserved as reasoning_content (never lost)"
        );
    }

    #[tokio::test]
    async fn partial_open_tag_at_eof_flushed_as_content() {
        // The stream is JUST a partial open tag that never completes — it was never a
        // confirmed block, so flush it verbatim as content (no loss, no misclassification).
        let seen = run_think_stream(&["<thi"]).await;
        let (content, reasoning) = joined(&seen);
        assert_eq!(
            content, "<thi",
            "ambiguous partial open tag flushed as content"
        );
        assert_eq!(reasoning, "");
    }

    #[tokio::test]
    async fn native_reasoning_model_untouched() {
        // A minimax-style model that streams native reasoning_content deltas must not be
        // transformed at all — its content (even if it contained the literal substring)
        // and reasoning_content pass through verbatim.
        use futures_util::StreamExt;
        let payload = json!({
            "id": "c1", "object": "chat.completion.chunk", "created": 0, "model": "MiniMax-TEE",
            "choices": [{ "index": 0, "delta": { "reasoning_content": "native thoughts", "content": "" } }],
        });
        let s = serde_json::to_string(&payload).unwrap();
        let chunk: crate::ChatCompletionChunk = serde_json::from_str(&s).unwrap();
        let ev = SSEEvent {
            raw_bytes: bytes::Bytes::from(format!("data: {s}\n\n")),
            chunk: Some(StreamChunk::Chat(chunk)),
            raw_passthrough: true,
        };
        let answer = content_chunk_event("the final answer");
        let decoded: StreamingResult = Box::pin(futures_util::stream::iter(vec![
            Ok(ev),
            Ok(answer),
            Ok(done_marker_event()),
        ]));
        let mut out = stream_with_think_extraction(decoded, None, false);
        let mut seen = Vec::new();
        while let Some(item) = out.next().await {
            let ev = item.unwrap();
            if ev.is_done_marker() {
                continue;
            }
            let s = std::str::from_utf8(&ev.raw_bytes).unwrap();
            let v: Value =
                serde_json::from_str(s.strip_prefix("data: ").unwrap().trim_end()).unwrap();
            let delta = &v["choices"][0]["delta"];
            seen.push((
                delta
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                delta
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ));
        }
        let (content, reasoning) = joined(&seen);
        assert_eq!(reasoning, "native thoughts", "native reasoning preserved");
        assert_eq!(
            content, "the final answer",
            "native model content untouched"
        );
    }

    #[tokio::test]
    async fn native_reasoning_after_held_whitespace_does_not_lose_content() {
        // Regression: a stream that first emits leading whitespace (held back while
        // scanning) and THEN reveals itself as a native-reasoning model must not drop
        // the held whitespace — it's ordinary content and must reappear.
        use futures_util::StreamExt;
        let ws = content_chunk_event("  "); // held back (could still open <think>)
        let native = {
            let p = json!({"id":"c1","object":"chat.completion.chunk","created":0,"model":"MiniMax-TEE","choices":[{"index":0,"delta":{"reasoning_content":"thoughts","content":"hi"}}]});
            let s = serde_json::to_string(&p).unwrap();
            SSEEvent {
                raw_bytes: bytes::Bytes::from(format!("data: {s}\n\n")),
                chunk: Some(StreamChunk::Chat(serde_json::from_str(&s).unwrap())),
                raw_passthrough: true,
            }
        };
        let decoded: StreamingResult = Box::pin(futures_util::stream::iter(vec![
            Ok(ws),
            Ok(native),
            Ok(done_marker_event()),
        ]));
        let mut out = stream_with_think_extraction(decoded, None, false);
        let mut seen = Vec::new();
        while let Some(item) = out.next().await {
            let ev = item.unwrap();
            if ev.is_done_marker() {
                continue;
            }
            let s = std::str::from_utf8(&ev.raw_bytes).unwrap();
            let v: Value =
                serde_json::from_str(s.strip_prefix("data: ").unwrap().trim_end()).unwrap();
            let delta = &v["choices"][0]["delta"];
            seen.push((
                delta
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                delta
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ));
        }
        let (content, reasoning) = joined(&seen);
        assert_eq!(content, "  hi", "held whitespace prepended, not lost");
        assert_eq!(reasoning, "thoughts", "native reasoning preserved");
    }

    #[tokio::test]
    async fn empty_think_block_emits_no_reasoning() {
        // `<think></think>answer` — empty block: stripped from content, but no spurious
        // empty reasoning_content (matches the non-stream "empty == unset" rule).
        let seen = run_think_stream(&["<think></think>answer"]).await;
        let (content, reasoning) = joined(&seen);
        assert_eq!(content, "answer");
        assert_eq!(
            reasoning, "",
            "empty think block does not set reasoning_content"
        );
    }

    #[tokio::test]
    async fn think_extraction_composes_with_canonical_rewrite_and_strip() {
        // End-to-end: think extraction + #787 canonical-id rewrite + internals strip all
        // compose. The slug must not leak, internals must be gone, and the extracted
        // reasoning must survive.
        use futures_util::StreamExt;
        let payload = r#"{"id":"c1","object":"chat.completion.chunk","created":0,"model":"Qwen/Qwen3-TEE","prompt_sha256":"x","choices":[{"index":0,"matched_stop":1,"delta":{"content":"<think>why</think>hi","token_ids":[1]}}]}"#;
        let chunk: crate::ChatCompletionChunk = serde_json::from_str(payload).unwrap();
        let ev = SSEEvent {
            raw_bytes: bytes::Bytes::from(format!("data: {payload}\n\n")),
            chunk: Some(StreamChunk::Chat(chunk)),
            raw_passthrough: true,
        };
        let decoded: StreamingResult = Box::pin(futures_util::stream::iter(vec![
            Ok(ev),
            Ok(done_marker_event()),
        ]));
        let mut out =
            stream_with_think_extraction(decoded, Some("Qwen/Qwen3-Coder".to_string()), false);
        let first = out.next().await.unwrap().unwrap();
        let s = std::str::from_utf8(&first.raw_bytes).unwrap();
        let v: Value = serde_json::from_str(s.strip_prefix("data: ").unwrap().trim_end()).unwrap();
        assert_eq!(v["model"], "Qwen/Qwen3-Coder", "canonical id applied");
        assert!(!s.contains("Qwen3-TEE"), "slug must not leak");
        assert!(
            v.get("prompt_sha256").is_none(),
            "top-level internal stripped"
        );
        assert!(
            v["choices"][0].get("matched_stop").is_none(),
            "choice internal stripped"
        );
        assert!(
            v["choices"][0]["delta"].get("token_ids").is_none(),
            "delta internal stripped"
        );
        assert_eq!(
            v["choices"][0]["delta"]["content"], "hi",
            "answer is clean content"
        );
        assert_eq!(
            v["choices"][0]["delta"]["reasoning_content"], "why",
            "reasoning extracted"
        );
        // Parsed chunk stays in lockstep with raw_bytes.
        if let Some(StreamChunk::Chat(c)) = &first.chunk {
            assert_eq!(c.model, "Qwen/Qwen3-Coder");
        } else {
            panic!("expected a Chat chunk");
        }
    }

    #[tokio::test]
    async fn role_and_finish_chunks_pass_through_during_think() {
        // A role-only opener and a finish-reason closer (no content) must pass through
        // untouched while a think block streams between them.
        use futures_util::StreamExt;
        let role = {
            let p = json!({"id":"c1","object":"chat.completion.chunk","created":0,"model":"Qwen/Qwen3-TEE","choices":[{"index":0,"delta":{"role":"assistant"}}]});
            let s = serde_json::to_string(&p).unwrap();
            SSEEvent {
                raw_bytes: bytes::Bytes::from(format!("data: {s}\n\n")),
                chunk: Some(StreamChunk::Chat(serde_json::from_str(&s).unwrap())),
                raw_passthrough: true,
            }
        };
        let finish = {
            let p = json!({"id":"c1","object":"chat.completion.chunk","created":0,"model":"Qwen/Qwen3-TEE","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
            let s = serde_json::to_string(&p).unwrap();
            SSEEvent {
                raw_bytes: bytes::Bytes::from(format!("data: {s}\n\n")),
                chunk: Some(StreamChunk::Chat(serde_json::from_str(&s).unwrap())),
                raw_passthrough: true,
            }
        };
        let decoded: StreamingResult = Box::pin(futures_util::stream::iter(vec![
            Ok(role),
            Ok(content_chunk_event("<think>r</think>a")),
            Ok(finish),
            Ok(done_marker_event()),
        ]));
        let mut out = stream_with_think_extraction(decoded, None, false);
        let mut contents = Vec::new();
        let mut reasonings = Vec::new();
        let mut saw_role = false;
        let mut saw_finish = false;
        while let Some(item) = out.next().await {
            let ev = item.unwrap();
            if ev.is_done_marker() {
                continue;
            }
            let s = std::str::from_utf8(&ev.raw_bytes).unwrap();
            let v: Value =
                serde_json::from_str(s.strip_prefix("data: ").unwrap().trim_end()).unwrap();
            let delta = &v["choices"][0]["delta"];
            if delta.get("role").is_some() {
                saw_role = true;
            }
            if v["choices"][0].get("finish_reason").and_then(Value::as_str) == Some("stop") {
                saw_finish = true;
            }
            if let Some(c) = delta.get("content").and_then(Value::as_str) {
                contents.push(c.to_string());
            }
            if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str) {
                reasonings.push(r.to_string());
            }
        }
        assert!(saw_role, "role-only opener forwarded");
        assert!(saw_finish, "finish_reason chunk forwarded");
        assert_eq!(contents.concat(), "a");
        assert_eq!(reasonings.concat(), "r");
    }
}
