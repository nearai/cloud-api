//! Chutes — an attested third-party inference provider (`ProviderTier::Attested3p`).
//!
//! Chutes (chutes.ai) serves models from Intel TDX + NVIDIA confidential-compute
//! TEEs. Its **data path** is OpenAI-compatible (`https://llm.chutes.ai/v1`), so
//! this provider reuses the existing OpenAI-compatible backend for chat / image /
//! audio / etc., exactly like NEAR's external providers.
//!
//! Its **attestation path** is what makes it `Attested3p` rather than
//! `NonAttested`: a later PR wires a `ChutesBackendVerifier` that fetches Chutes'
//! `/evidence`, transforms the TDX quote + GPU evidence into NEAR's attestation
//! format, and runs the shared verifier under the strict `Attested3p` policy
//! (`MeasurementPolicy::attested3p` + `StrictBoundReportDataVerifier`).
//!
//! # Status — skeleton, behind a hard-off gate
//!
//! This is the **data-path skeleton** (PR5). It is **not constructed anywhere**:
//! nothing in the provider pool builds a `chutes::Provider` yet, so it cannot
//! serve traffic. The pool wiring + an explicit enable flag land with the
//! attestation verifier (PR7); turning Chutes on in production is gated behind
//! that flag until the open questions about Chutes' evidence format are
//! confirmed. Until then `get_attestation_report` / `get_signature` return an
//! explicit "not yet wired" error so this provider can never be mistaken for a
//! verified one.

use async_trait::async_trait;
use std::sync::Arc;

use crate::non_attested::external::{ExternalProvider, ExternalProviderConfig, ProviderConfig};
use crate::{
    AttestationError, AudioTranscriptionError, AudioTranscriptionParams,
    AudioTranscriptionResponse, ChatCompletionParams, ChatCompletionResponseWithBytes,
    ChatSignature, CompletionError, CompletionParams, EmbeddingError, ImageEditError,
    ImageEditParams, ImageEditResponseWithBytes, ImageGenerationError, ImageGenerationParams,
    ImageGenerationResponseWithBytes, InferenceProvider, ListModelsError, ModelsResponse,
    PrivacyClassifyError, RerankError, RerankParams, RerankResponse, ScoreError, ScoreParams,
    ScoreResponse, StreamingResult,
};

/// Default Chutes OpenAI-compatible inference base URL.
pub const DEFAULT_BASE_URL: &str = "https://llm.chutes.ai/v1";

/// Configuration for a Chutes attested provider.
///
/// Fields are **private** and the only constructor is [`Config::new`], so the
/// `api_key` secret cannot be exposed by a derived `Debug` (see the custom,
/// redacting `Debug` impl below) and `timeout_seconds` cannot be set to a
/// negative value that would underflow the `as u64` cast in the HTTP layer.
///
/// The attestation-specific fields (chute id, `/evidence` endpoint, golden
/// measurements) are added by the PR that wires the verifier; this skeleton only
/// needs what the OpenAI-compatible data path uses.
#[derive(Clone)]
pub struct Config {
    /// OpenAI-compatible inference base URL (e.g. `https://llm.chutes.ai/v1`).
    base_url: String,
    /// Chutes API key (`cpk_...`). A secret — sourced from env/secret store,
    /// never hard-coded or logged (redacted in `Debug`).
    api_key: String,
    /// The model id as served by Chutes (e.g. `zai-org/GLM-5.1-TEE`).
    model_name: String,
    /// Per-request timeout, seconds. Always > 0 (see `new`).
    timeout_seconds: i64,
}

/// Sane fallback when a non-positive timeout is supplied (matches the external
/// provider default); a non-positive value would otherwise underflow the
/// `timeout_seconds as u64` cast in the HTTP backend into an effectively
/// infinite timeout.
const DEFAULT_TIMEOUT_SECONDS: i64 = 300;

impl Config {
    /// Build a config pointing at Chutes' default inference URL. A non-positive
    /// `timeout_seconds` is replaced with [`DEFAULT_TIMEOUT_SECONDS`].
    pub fn new(api_key: String, model_name: String, timeout_seconds: i64) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key,
            model_name,
            timeout_seconds: if timeout_seconds > 0 {
                timeout_seconds
            } else {
                DEFAULT_TIMEOUT_SECONDS
            },
        }
    }

    /// Override the inference base URL (defaults to [`DEFAULT_BASE_URL`]). Used
    /// by integration tests / mock servers and by the verifier-wiring PR, which
    /// sources the URL from config rather than hardcoding it.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// The OpenAI-compatible inference base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The per-request timeout in seconds (always > 0).
    pub fn timeout_seconds(&self) -> i64 {
        self.timeout_seconds
    }
}

// Manual `Debug` that redacts the API key. Never derive `Debug` on a struct
// holding a secret — an accidental `{:?}` (init/error log) would leak it.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("base_url", &self.base_url)
            .field("api_key", &"[redacted]")
            .field("model_name", &self.model_name)
            .field("timeout_seconds", &self.timeout_seconds)
            .finish()
    }
}

/// Chutes attested inference provider.
///
/// The OpenAI-compatible data path is delegated to an inner [`ExternalProvider`]
/// (proven, shared with NEAR's external providers). Only the attestation methods
/// are Chutes-specific — stubbed here, made real when the verifier is wired.
pub struct Provider {
    /// Inner OpenAI-compatible client for the data path (chat/image/audio/...).
    data_path: ExternalProvider,
}

impl Provider {
    pub fn new(config: Config) -> Self {
        let Config {
            base_url,
            api_key,
            model_name,
            timeout_seconds,
        } = config;

        let data_path = ExternalProvider::new(ExternalProviderConfig {
            model_name,
            provider_config: ProviderConfig::OpenAiCompatible {
                base_url,
                organization_id: None,
                model_name: None,
                extra_request_body: None,
            },
            api_key,
            timeout_seconds,
        });

        Self { data_path }
    }

    /// Model id this provider serves.
    pub fn model_name(&self) -> &str {
        self.data_path.model_name()
    }
}

#[async_trait]
impl InferenceProvider for Provider {
    // ---- Data path: delegated to the OpenAI-compatible backend ----

    async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        self.data_path.models().await
    }

    async fn chat_completion(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        self.data_path.chat_completion(params, request_hash).await
    }

    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        self.data_path
            .chat_completion_stream(params, request_hash)
            .await
    }

    async fn text_completion_stream(
        &self,
        params: CompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        self.data_path.text_completion_stream(params).await
    }

    async fn image_generation(
        &self,
        params: ImageGenerationParams,
        request_hash: String,
    ) -> Result<ImageGenerationResponseWithBytes, ImageGenerationError> {
        self.data_path.image_generation(params, request_hash).await
    }

    async fn image_edit(
        &self,
        params: Arc<ImageEditParams>,
        request_hash: String,
    ) -> Result<ImageEditResponseWithBytes, ImageEditError> {
        self.data_path.image_edit(params, request_hash).await
    }

    async fn audio_transcription(
        &self,
        params: AudioTranscriptionParams,
        request_hash: String,
    ) -> Result<AudioTranscriptionResponse, AudioTranscriptionError> {
        self.data_path
            .audio_transcription(params, request_hash)
            .await
    }

    async fn score(
        &self,
        params: ScoreParams,
        request_hash: String,
    ) -> Result<ScoreResponse, ScoreError> {
        self.data_path.score(params, request_hash).await
    }

    async fn rerank(&self, params: RerankParams) -> Result<RerankResponse, RerankError> {
        self.data_path.rerank(params).await
    }

    async fn embeddings_raw(
        &self,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, EmbeddingError> {
        self.data_path.embeddings_raw(body, extra).await
    }

    async fn privacy_classify_raw(
        &self,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, PrivacyClassifyError> {
        self.data_path.privacy_classify_raw(body, extra).await
    }

    // ---- Attestation path: Chutes-specific, wired in a later PR ----
    //
    // Returns an explicit "not yet wired" error rather than delegating to the
    // inner external provider (whose message says attestation is *unsupported*).
    // A later PR replaces these with the real Chutes verifier; until then this
    // provider must never appear verified.

    async fn get_attestation_report(
        &self,
        _model: String,
        _signing_algo: Option<String>,
        _nonce: Option<String>,
        _signing_address: Option<String>,
        _include_tls_fingerprint: bool,
    ) -> Result<serde_json::Map<String, serde_json::Value>, AttestationError> {
        Err(AttestationError::FetchError(
            "Chutes attestation is not yet wired (provider is behind a hard-off gate); \
             the /evidence transform + verifier land in a later PR"
                .to_string(),
        ))
    }

    async fn get_signature(
        &self,
        _chat_id: &str,
        _signing_algo: Option<String>,
    ) -> Result<ChatSignature, CompletionError> {
        Err(CompletionError::CompletionError(
            "Chutes signature retrieval is not yet wired (provider is behind a hard-off gate)"
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_points_at_chutes_and_keeps_model() {
        let p = Provider::new(Config::new(
            "cpk_test".to_string(),
            "zai-org/GLM-5.1-TEE".to_string(),
            300,
        ));
        assert_eq!(p.model_name(), "zai-org/GLM-5.1-TEE");
    }

    #[test]
    fn config_default_base_url() {
        let c = Config::new("k".to_string(), "m".to_string(), 60);
        assert_eq!(c.base_url(), "https://llm.chutes.ai/v1");
    }

    #[test]
    fn config_non_positive_timeout_falls_back_to_default() {
        // Guards the `timeout_seconds as u64` cast in the HTTP backend. Assert on
        // the accessor, not Debug output, so formatting changes don't break it.
        assert_eq!(
            Config::new("k".into(), "m".into(), -5).timeout_seconds(),
            DEFAULT_TIMEOUT_SECONDS
        );
        assert_eq!(
            Config::new("k".into(), "m".into(), 0).timeout_seconds(),
            DEFAULT_TIMEOUT_SECONDS
        );
        assert_eq!(
            Config::new("k".into(), "m".into(), 42).timeout_seconds(),
            42
        );
    }

    #[test]
    fn config_debug_redacts_api_key() {
        // Use a realistic-length, distinctive secret so the leak check is meaningful.
        let secret = "cpk_super_secret_value_0123456789abcdef";
        let redacted = format!("{:?}", Config::new(secret.into(), "m".into(), 60));
        assert!(redacted.contains("[redacted]"), "key must be redacted");
        assert!(
            !redacted.contains(secret),
            "api_key must never appear in Debug output, got: {redacted}"
        );
    }

    #[test]
    fn with_base_url_overrides_default() {
        let c = Config::new("k".into(), "m".into(), 60).with_base_url("http://127.0.0.1:9999/v1");
        assert_eq!(c.base_url(), "http://127.0.0.1:9999/v1");
        // And the override survives into the provider.
        let p = Provider::new(c);
        assert_eq!(p.model_name(), "m");
    }

    #[tokio::test]
    async fn attestation_methods_error_until_wired() {
        let p = Provider::new(Config::new("k".to_string(), "m".to_string(), 60));

        // Match the concrete error variant + message (not the Debug string).
        match p
            .get_attestation_report("m".to_string(), None, None, None, true)
            .await
        {
            Err(AttestationError::FetchError(msg)) => assert!(msg.contains("not yet wired")),
            other => panic!("expected AttestationError::FetchError, got {other:?}"),
        }

        match p.get_signature("chat-1", None).await {
            Err(CompletionError::CompletionError(msg)) => assert!(msg.contains("not yet wired")),
            other => panic!("expected CompletionError::CompletionError, got {other:?}"),
        }
    }
}
