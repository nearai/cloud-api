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
/// The attestation-specific fields (chute id, `/evidence` endpoint, golden
/// measurements) are added by the PR that wires the verifier; this skeleton only
/// needs what the OpenAI-compatible data path uses.
#[derive(Debug, Clone)]
pub struct Config {
    /// OpenAI-compatible inference base URL (e.g. `https://llm.chutes.ai/v1`).
    pub base_url: String,
    /// Chutes API key (`cpk_...`). A secret — sourced from env/secret store,
    /// never hard-coded or logged.
    pub api_key: String,
    /// The model id as served by Chutes (e.g. `zai-org/GLM-5.1-TEE`).
    pub model_name: String,
    /// Per-request timeout, seconds.
    pub timeout_seconds: i64,
}

impl Config {
    /// Build a config pointing at Chutes' default inference URL.
    pub fn new(api_key: String, model_name: String, timeout_seconds: i64) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key,
            model_name,
            timeout_seconds,
        }
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
        assert_eq!(c.base_url, "https://llm.chutes.ai/v1");
    }

    #[tokio::test]
    async fn attestation_methods_error_until_wired() {
        let p = Provider::new(Config::new("k".to_string(), "m".to_string(), 60));

        let att = p
            .get_attestation_report("m".to_string(), None, None, None, true)
            .await;
        assert!(att.is_err(), "attestation must not appear available yet");
        assert!(format!("{:?}", att.unwrap_err()).contains("not yet wired"));

        let sig = p.get_signature("chat-1", None).await;
        assert!(sig.is_err(), "signature must not appear available yet");
        assert!(format!("{:?}", sig.unwrap_err()).contains("not yet wired"));
    }
}
