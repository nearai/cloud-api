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
    RerankError, RerankParams, RerankResponse, ScoreError, ScoreParams, ScoreResponse,
    StreamingResult,
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
    /// The model id as served by Chutes (e.g. `zai-org/GLM-5.1-TEE`).
    model_name: String,
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
    /// Whether the streaming chat path is exposed as attested (see
    /// [`Config::allow_streaming`]).
    allow_streaming: bool,
    /// Memoized model→`chute_id` (the mapping is static), so we don't re-fetch
    /// `/v1/models` on every request.
    chute_id_cache: tokio::sync::OnceCell<String>,
}

/// Everything needed to invoke a verified instance: the targeting headers, the
/// E2EE request blob, and the session to decrypt the reply.
struct PreparedInvoke {
    chute_id: String,
    instance_id: String,
    nonce_token: String,
    blob: Vec<u8>,
    session: e2ee::ResponseSession,
    /// Matched golden config (for logging/annotation).
    #[allow(dead_code)]
    measurement_config: String,
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
            allow_streaming,
            chute_id_cache: tokio::sync::OnceCell::new(),
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

    /// Discover → fetch evidence → **verify** a Chutes instance, then build the
    /// E2EE request blob for `request_json`. Returns an error (never an
    /// unverified channel) if any stage fails.
    async fn verify_and_prepare(&self, request_json: &Value) -> Result<PreparedInvoke, String> {
        // Cached: the model→chute_id mapping is static, so resolve once.
        let chute_id = self
            .chute_id_cache
            .get_or_try_init(|| self.client.resolve_chute_id(&self.model_name))
            .await
            .map_err(|e| format!("resolve chute_id: {e}"))?
            .clone();

        let instances = self
            .client
            .discover_instances(&chute_id)
            .await
            .map_err(|e| format!("discover instances: {e}"))?;

        // Candidate instances: live + E2E-capable + with at least one nonce token.
        let candidates: Vec<&client::E2eInstance> = instances
            .instances
            .iter()
            .filter(|i| !i.e2e_pubkey.is_empty() && !i.nonces.is_empty())
            .collect();
        if candidates.is_empty() {
            return Err("no E2E-capable Chutes instance with an available nonce token".to_string());
        }

        // One chute-wide /evidence fetch bound to a fresh boot nonce; every
        // instance's report_data binds this same nonce + its own e2e_pubkey.
        let boot_nonce = Self::random_boot_nonce()?;
        let evidence_resp = self
            .client
            .fetch_evidence(&chute_id, &boot_nonce)
            .await
            .map_err(|e| format!("fetch evidence: {e}"))?;

        // Try each candidate until one verifies, so a single bad/unverifiable
        // instance doesn't take down all requests. Verification failure is never
        // a fallback to an unverified channel — we just move to the next attested
        // candidate, and fail if none verify.
        let mut last_err = String::from("no candidate instances");
        for inst in candidates {
            let evidence = match evidence_resp.instance(&inst.instance_id) {
                Some(e) => e,
                None => {
                    last_err = format!("instance {} not present in /evidence", inst.instance_id);
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
                    continue;
                }
            };
            let e2e_pk = match base64::engine::general_purpose::STANDARD.decode(e2e_pubkey) {
                Ok(pk) => pk,
                Err(e) => {
                    last_err = format!("instance {} e2e_pubkey base64: {e}", inst.instance_id);
                    continue;
                }
            };
            let prepared = match e2ee::build_request(&e2e_pk, request_json) {
                Ok(p) => p,
                Err(e) => {
                    last_err = format!("instance {} E2EE build: {e}", inst.instance_id);
                    continue;
                }
            };
            let e2ee::PreparedRequest { blob, session } = prepared;
            return Ok(PreparedInvoke {
                chute_id,
                instance_id: inst.instance_id.clone(),
                // Random token (not always [0]) to reduce single-use-token
                // collisions between concurrent requests to the same instance.
                nonce_token: pick_nonce(&inst.nonces).to_string(),
                blob,
                session,
                measurement_config: info.measurement_config,
            });
        }
        Err(format!(
            "all candidate Chutes instances failed (refusing to send inference); last: {last_err}"
        ))
    }
}

/// Pick a pseudo-random nonce token from a non-empty list, to reduce single-use
/// token collisions between concurrent requests. Falls back to the first token
/// if the OS RNG is briefly unavailable (still correct, just less collision-shy).
fn pick_nonce(nonces: &[String]) -> &str {
    debug_assert!(!nonces.is_empty());
    // Wrapping global counter — infallible and contention-correct: concurrent
    // requests to the same instance get distinct indices (unlike an RNG with a
    // fixed `[0]` fallback, which would collide exactly when it matters).
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let idx = COUNTER.fetch_add(1, Ordering::Relaxed) % nonces.len();
    &nonces[idx]
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
            obj.insert(
                "stream_options".to_string(),
                json!({ "include_usage": true }),
            );
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
        // The model set is configured explicitly; advertise just this one.
        Ok(ModelsResponse {
            object: "list".to_string(),
            data: vec![ModelInfo {
                created: 0,
                id: self.model_name.clone(),
                object: "model".to_string(),
                owned_by: "chutes".to_string(),
            }],
        })
    }

    async fn chat_completion(
        &self,
        params: ChatCompletionParams,
        _request_hash: String,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        reject_client_e2ee(&params)?;
        let body = request_body(&self.model_name, &params, false)
            .map_err(CompletionError::CompletionError)?;
        let prep = self
            .verify_and_prepare(&body)
            .await
            .map_err(CompletionError::CompletionError)?;

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
            .map_err(|e| CompletionError::CompletionError(format!("Chutes /e2e/invoke: {e}")))?;

        let plaintext = prep
            .session
            .decrypt_response(&resp_blob)
            .map_err(|e| CompletionError::CompletionError(format!("E2EE decrypt: {e}")))?;
        let response: ChatCompletionResponse = serde_json::from_slice(&plaintext)
            .map_err(|e| CompletionError::CompletionError(format!("parse response: {e}")))?;
        let raw_bytes = serde_json::to_vec(&response)
            .map_err(|e| CompletionError::CompletionError(format!("re-serialize: {e}")))?;
        Ok(ChatCompletionResponseWithBytes {
            response,
            raw_bytes,
        })
    }

    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
        _request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        // Streaming is off by default: Chutes' stream protocol has no
        // authenticated frame ordering (a gateway could drop/reorder AEAD-valid
        // frames undetectably), so it isn't honestly attested until Chutes adds
        // sequence numbers and the inner-terminator behavior is confirmed on
        // staging. Opt in with CHUTES_ENABLE_STREAMING. Non-streaming is attested.
        if !self.allow_streaming {
            return Err(CompletionError::CompletionError(
                "Chutes streaming is not enabled as an attested path (frame ordering is not \
                 cryptographically authenticated); use non-streaming, or set \
                 CHUTES_ENABLE_STREAMING to opt in"
                    .to_string(),
            ));
        }
        reject_client_e2ee(&params)?;
        let body = request_body(&self.model_name, &params, true)
            .map_err(CompletionError::CompletionError)?;
        let prep = self
            .verify_and_prepare(&body)
            .await
            .map_err(CompletionError::CompletionError)?;

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
            .map_err(|e| {
                CompletionError::CompletionError(format!("Chutes /e2e/invoke (stream): {e}"))
            })?;

        // Decrypt the E2EE SSE into OpenAI SSEEvents (transport errors → CompletionError).
        let byte_stream = resp.bytes_stream().map(|r| {
            r.map_err(|e| CompletionError::CompletionError(format!("Chutes stream transport: {e}")))
        });
        Ok(e2ee_stream::decrypt_e2ee_sse(
            Box::pin(byte_stream),
            prep.session,
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
        let chute_id = self
            .client
            .resolve_chute_id(&self.model_name)
            .await
            .map_err(|e| AttestationError::FetchError(format!("resolve chute_id: {e}")))?;
        let instances = self
            .client
            .discover_instances(&chute_id)
            .await
            .map_err(|e| AttestationError::FetchError(format!("discover instances: {e}")))?;
        let inst = instances
            .instances
            .iter()
            .find(|i| !i.e2e_pubkey.is_empty())
            .ok_or_else(|| {
                AttestationError::FetchError("no E2E-capable Chutes instance".to_string())
            })?;

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
        let evidence = evidence_resp.instance(&inst.instance_id).ok_or_else(|| {
            AttestationError::FetchError("chosen instance not present in /evidence".to_string())
        })?;

        // Trim to match the data path's canonicalization (verify_and_prepare).
        let e2e_pubkey = inst.e2e_pubkey.trim();
        let info = self
            .verifier
            .attest_instance(evidence, &boot_nonce, e2e_pubkey)
            .await
            .map_err(|e| AttestationError::FetchError(format!("attestation failed: {e}")))?;

        // A self-describing, independently re-verifiable report: the verdict plus
        // the raw quote + cert so a client can recompute the bindings themselves.
        let mut m = serde_json::Map::new();
        m.insert("provider".to_string(), json!("chutes"));
        m.insert("verified".to_string(), json!(true));
        m.insert("model".to_string(), json!(self.model_name));
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
        Ok(m)
    }

    /// Chutes' response integrity is the ML-KEM E2EE channel's AEAD tag, not a
    /// per-response signature — so the attestation flow skips the signature
    /// fetch/store step entirely (rather than calling `get_signature` and
    /// erroring on every completion).
    fn supports_chat_signatures(&self) -> bool {
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
}
