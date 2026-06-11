//! Inference providers crate for handling multiple AI inference backends
//!
//! This crate provides a streaming-first trait interface for different inference providers,
//! enabling seamless switching between different AI model backends.
//!
//! # Streaming-First Design
//!
//! All completion methods return streams of chunks rather than complete responses.
//! This design choice provides several benefits:
//!
//! - **Consistent API**: All methods behave the same way
//! - **Real-time UX**: Stream chunks as they arrive for better user experience  
//! - **Flexibility**: Clients can choose to block or stream based on their needs
//! - **Reduced complexity**: Single completion path rather than dual sync/async APIs
//!
//! # Usage
//!
//! ```rust,ignore
//! use inference_providers::{InferenceProvider, ChatCompletionParams};
//! use futures_util::StreamExt;
//!
//! async fn example<P: InferenceProvider>(provider: P) {
//!     let params = ChatCompletionParams {
//!         model: "gpt-4".to_string(),
//!         messages: vec![/* your messages */],
//!         max_completion_tokens: Some(100),
//!         temperature: Some(0.7),
//!         stream: Some(true),
//!         // ... other parameters
//!     };
//!
//!     // Stream chunks as they arrive
//!     let mut stream = provider.chat_completion_stream(params).await?;
//!     while let Some(chunk) = stream.next().await {
//!         match chunk {
//!             Ok(StreamChunk::Chat(chat_chunk)) => {
//!                 if chat_chunk.usage.is_some() {
//!                     println!("Final chunk with usage: {:?}", chat_chunk.usage);
//!                 } else if let Some(choice) = chat_chunk.choices.first() {
//!                     if let Some(delta) = &choice.delta {
//!                         println!("Delta content: {:?}", delta.content);
//!                     }
//!                 }
//!             }
//!             Ok(StreamChunk::Text(text_chunk)) => {
//!                 if let Some(choice) = text_chunk.choices.first() {
//!                     println!("Text content: {}", choice.text);
//!                 }
//!             }
//!             Err(e) => eprintln!("Stream error: {}", e),
//!         }
//!     }
//! }
//! ```

use reqwest::Client;

pub mod attested;
pub mod bucket_keepalive;
pub mod chunk_builder;
pub mod mock;
pub mod models;
pub mod non_attested;
pub mod rotation;
pub mod spki_verifier;
pub mod sse_parser;

// Attested NEAR-AI fleet provider. Use the module path (`nearai::Provider`,
// `nearai::Config`) rather than a bare re-export to keep the names unambiguous.
pub use attested::nearai;

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_core::Stream;
use models::*;
use tokio_stream::StreamExt;

// Re-export commonly used types for convenience
pub use mock::MockProvider;
pub use models::{
    AudioOutput, AudioTranscriptionError, AudioTranscriptionParams, AudioTranscriptionResponse,
    ChatCompletionParams, ChatCompletionResponse, ChatCompletionResponseChoice,
    ChatCompletionResponseWithBytes, ChatDelta, ChatMessage, ChatResponseMessage, ChatSignature,
    CompletionError, CompletionParams, EmbeddingError, FinishReason, FunctionChoice,
    FunctionDefinition, ImageData, ImageEditError, ImageEditParams, ImageEditResponse,
    ImageEditResponseWithBytes, ImageGenerationError, ImageGenerationParams,
    ImageGenerationResponse, ImageGenerationResponseWithBytes, MessageRole, ModelInfo,
    PrivacyClassifyError, RerankError, RerankParams, RerankResponse, RerankResult, RerankUsage,
    ScoreError, ScoreParams, ScoreResponse, ScoreResult, ScoreUsage, StreamChunk, StreamOptions,
    TokenUsage, ToolChoice, ToolDefinition, TranscriptionSegment, TranscriptionWord,
};
pub use sse_parser::{
    new_external_sse_parser, new_sse_parser, BufferedSSEParser, SSEEvent, SSEEventParser, SSEParser,
};
// Chunk builder for external provider parsers
pub use chunk_builder::ChunkContext;

// Non-attested (third-party) provider exports
pub use non_attested::external::{
    AnthropicBackend, ExternalProvider, ExternalProviderConfig, GeminiBackend,
    OpenAiCompatibleBackend, ProviderConfig,
};

/// Trust tier of an inference provider, along the attestation axis introduced in
/// the `attested::` / `non_attested::` module split.
///
/// This is the taxonomy the inference stack branches on — *can we cryptographically
/// verify what served the response?* — not the engine or who operates the backend:
///
/// - [`ProviderTier::Near`] — NEAR AI's own TEE fleet ([`attested::nearai`]). Full
///   chain: per-request TDX quote + GPU evidence + TLS-SPKI pin + per-response
///   signature, all rooted in NEAR's own KMS / compose-manager / gateway.
/// - [`ProviderTier::Attested3p`] — a third party that produces a *verifiable* TEE
///   attestation we can bind to the response (e.g. Chutes, future siblings of
///   `attested::nearai`). Earns the "attested" badge **only** when the per-response
///   bindings that NEAR's verifier requires (fresh nonce, signing key, TLS
///   fingerprint, authenticated measurement) are present; otherwise it is
///   [`ProviderTier::NonAttested`].
/// - [`ProviderTier::NonAttested`] — plaintext third parties
///   ([`non_attested::external`]: OpenAI / Anthropic / Gemini / OpenRouter) with no
///   verifiable attestation. No "verified" badge.
///
/// PR1 of the Chutes integration: this enum is the scaffolding that later PRs use to
/// select a per-provider measurement policy and report-data verifier at pool
/// construction time. It carries no behavior on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderTier {
    /// NEAR AI's own attested TEE fleet (`attested::nearai`).
    Near,
    /// A third party with a verifiable, response-bound TEE attestation
    /// (`attested::<provider>`, e.g. Chutes).
    Attested3p,
    /// A plaintext third party with no verifiable attestation
    /// (`non_attested::external`).
    NonAttested,
}

impl ProviderTier {
    /// Whether this tier carries a verifiable TEE attestation we gate a
    /// "verified" badge on. True for [`Near`](ProviderTier::Near) and
    /// [`Attested3p`](ProviderTier::Attested3p); false for
    /// [`NonAttested`](ProviderTier::NonAttested).
    pub fn is_attested(self) -> bool {
        matches!(self, ProviderTier::Near | ProviderTier::Attested3p)
    }
}

/// Creates a verified `reqwest::Client` with an H2 connection to a specific backend.
///
/// Used by `nearai::Provider` for inline backend verification: when a bucket needs a new
/// client, the verifier connects to model-proxy, fetches the backend's attestation
/// report, verifies it (TDX quote, GPU evidence, image hash), pins the TLS fingerprint,
/// and returns the client with its established connection.
#[async_trait::async_trait]
pub trait BackendVerifier: Send + Sync {
    /// Connect to `base_url`, verify the backend's attestation, and return a client
    /// whose H2 connection is pinned to that verified backend.
    async fn create_verified_client(&self, base_url: &str) -> Result<Client, String>;
}

/// Try to extract a human-readable error message from a JSON error response body.
///
/// Supports common formats:
///   - OpenAI/Anthropic: `{"error": {"message": "..."}}`
///   - vLLM flat: `{"object": "error", "message": "...", "type": "..."}`
///   - vLLM/FastAPI: `{"detail": "..."}`
///   - Falls back to the raw body if none match
pub fn extract_error_message(body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        // OpenAI/Anthropic format: {"error": {"message": "..."}}
        if let Some(msg) = json
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return msg.to_string();
        }
        // vLLM flat format: {"object":"error","message":"...","type":"..."}
        // Distinguished from envelope JSON by the top-level `message` field
        // (we don't want to pick up `message` fields nested deep elsewhere).
        if let Some(msg) = json.get("message").and_then(|m| m.as_str()) {
            return msg.to_string();
        }
        // FastAPI format: {"detail": "..."}
        if let Some(detail) = json.get("detail").and_then(|d| d.as_str()) {
            return detail.to_string();
        }
    }
    body.to_string()
}

/// Type alias for streaming completion results
///
/// This represents a stream of SSE events where each event contains:
/// - `raw_bytes` - The exact bytes received from the source (for forwarding)
/// - `chunk` - The parsed StreamChunk for processing
pub type StreamingResult = Pin<Box<dyn Stream<Item = Result<SSEEvent, CompletionError>> + Send>>;

/// Type alias for peekable streaming completion results
pub type PeekableStreamingResult = tokio_stream::adapters::Peekable<StreamingResult>;

/// Extension trait to add peekable functionality to StreamingResult
pub trait StreamingResultExt {
    /// Convert this streaming result into a peekable stream
    fn peekable(self) -> PeekableStreamingResult;
}

impl StreamingResultExt for StreamingResult {
    fn peekable(self) -> PeekableStreamingResult {
        StreamExt::peekable(self)
    }
}

#[async_trait]
pub trait InferenceProvider {
    /// Lists all available models from this provider
    ///
    /// Returns a list of `ModelInfo` structs containing model details like ID, name,
    /// description, and context length.
    async fn models(&self) -> Result<ModelsResponse, ListModelsError>;

    /// Performs a streaming chat completion request
    ///
    /// Returns a stream of `StreamChunk` objects that can be processed incrementally
    /// to provide real-time responses to users. The stream will emit chunks as they
    /// become available from the underlying provider.
    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<StreamingResult, CompletionError>;

    async fn chat_completion(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError>;

    /// Performs a streaming text completion request
    ///
    /// Returns a stream of `StreamChunk` objects for incremental text generation.
    /// Similar to chat completion but for raw text prompts rather than conversations.
    async fn text_completion_stream(
        &self,
        params: CompletionParams,
    ) -> Result<StreamingResult, CompletionError>;

    /// Performs an image generation request
    ///
    /// Returns generated images based on the provided text prompt.
    /// Includes raw bytes for TEE signature verification.
    async fn image_generation(
        &self,
        params: ImageGenerationParams,
        request_hash: String,
    ) -> Result<ImageGenerationResponseWithBytes, ImageGenerationError>;

    /// Performs an image edit request
    ///
    /// Returns edited images based on the provided image and prompt.
    /// Includes raw bytes for TEE signature verification.
    /// Accepts Arc<ImageEditParams> to avoid unnecessary cloning during retries (image data is already Arc-wrapped).
    async fn image_edit(
        &self,
        params: Arc<ImageEditParams>,
        request_hash: String,
    ) -> Result<ImageEditResponseWithBytes, ImageEditError>;

    /// Performs a document reranking request
    ///
    /// Returns documents reranked by relevance to the provided query.
    /// Returns scored and ranked results.
    /// Performs a text similarity scoring request
    ///
    /// Compares two texts and returns a similarity score using a reranker model.
    async fn score(
        &self,
        params: ScoreParams,
        request_hash: String,
    ) -> Result<ScoreResponse, ScoreError>;

    async fn rerank(&self, params: RerankParams) -> Result<RerankResponse, RerankError>;

    /// Performs an embeddings request as a raw passthrough.
    ///
    /// Accepts the raw JSON request body and returns the raw JSON response bytes.
    /// No deserialization is performed — the cloud API proxies the request as-is.
    async fn embeddings_raw(
        &self,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, EmbeddingError>;

    /// Performs a privacy classification request as a raw passthrough.
    ///
    /// Accepts the raw JSON request body and returns the raw JSON response bytes.
    /// No deserialization is performed — the cloud API proxies the request as-is to
    /// the backend's `/v1/privacy/classify` endpoint.
    async fn privacy_classify_raw(
        &self,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, PrivacyClassifyError>;

    async fn get_signature(
        &self,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<ChatSignature, CompletionError>;

    /// Pin a dedicated TLS connection to a chat_id for signature fetching.
    /// Called by the pool after peeking the chat_id from the stream.
    /// For L4 load-balanced backends, this ensures the signature fetch
    /// goes over the same TLS connection that served the completion.
    fn pin_chat_connection(&self, _request_hash: &str, _chat_id: &str) {}

    /// Whether this provider exposes per-response chat signatures via
    /// [`Self::get_signature`]. Default `true` (the historical behavior). A
    /// provider whose response integrity is the transport itself — e.g. Chutes,
    /// where it's the ML-KEM E2EE channel's AEAD tag rather than a signed
    /// response — returns `false`, so the attestation flow skips the
    /// signature-fetch/store step instead of erroring on every completion.
    fn supports_chat_signatures(&self) -> bool {
        true
    }

    /// The trust tier of this provider — see [`ProviderTier`]. Drives provider
    /// ordering in the pool (NEAR-served models prefer their own attested fleet
    /// and fall back to an attested third party like Chutes only when the NEAR
    /// backends can't fulfill the request). Default [`ProviderTier::NonAttested`];
    /// `attested::nearai` returns [`ProviderTier::Near`] and `attested::chutes`
    /// returns [`ProviderTier::Attested3p`].
    fn tier(&self) -> ProviderTier {
        ProviderTier::NonAttested
    }

    /// Whether this provider can serve **streaming** completions. Default `true`.
    /// A provider that gates streaming (e.g. Chutes when `CHUTES_ENABLE_STREAMING`
    /// is off — its stream protocol has no authenticated frame ordering) returns
    /// `false`, so the pool prefers a streaming-capable sibling for streaming
    /// requests instead of falling through to a hard "streaming not enabled" error
    /// that would mask the primary's failure and suppress its retry.
    fn supports_streaming(&self) -> bool {
        true
    }

    /// Whether this provider can serve a request carrying **client-facing E2EE**
    /// intent (the client asked cloud-api to encrypt the response to its own key,
    /// via `x_client_pub_key`). Default `true`. A provider that can't (e.g. Chutes,
    /// whose responses arrive over its own ML-KEM channel and which rejects the
    /// client encryption headers) returns `false`, so the pool prefers a capable
    /// sibling for such requests instead of falling through to a hard rejection
    /// that masks the primary's failure and suppresses its retry.
    fn supports_client_e2ee(&self) -> bool {
        true
    }

    /// Clean up the dedicated client for a chat_id after signature fetching.
    fn unpin_chat_connection(&self, _chat_id: &str) {}

    /// Update the provider's view of how many healthy backends sit behind its
    /// canonical SNI. Discovery calls this after each cycle so the traffic-
    /// time fallback path knows how many rotation-SNI indices to iterate when
    /// the sticky backend returns a 5xx. Default is a no-op — only providers
    /// that participate in model-proxy rotation (vLLM) override it.
    fn set_backend_count(&self, _count: usize) {}

    async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
    ) -> Result<serde_json::Map<String, serde_json::Value>, AttestationError>;

    /// Performs an audio transcription request
    ///
    /// Accepts audio file bytes and returns transcription with word-level timing,
    /// segments, and metadata using Whisper models.
    async fn audio_transcription(
        &self,
        params: AudioTranscriptionParams,
        request_hash: String,
    ) -> Result<AudioTranscriptionResponse, AudioTranscriptionError>;
}

#[cfg(test)]
mod provider_tier_tests {
    use super::ProviderTier;

    #[test]
    fn attested_tiers_gate_the_verified_badge() {
        assert!(ProviderTier::Near.is_attested());
        assert!(ProviderTier::Attested3p.is_attested());
        assert!(!ProviderTier::NonAttested.is_attested());
    }
}

#[cfg(test)]
mod extract_error_message_tests {
    use super::extract_error_message;

    #[test]
    fn test_openai_nested_format() {
        let body = r#"{"error":{"message":"Invalid API key","type":"auth_error"}}"#;
        assert_eq!(extract_error_message(body), "Invalid API key");
    }

    #[test]
    fn test_vllm_flat_format() {
        // vLLM/sglang emit this shape for validation errors.
        let body = r#"{"object":"error","message":"dimensions parameter is not supported for this model","type":"BadRequestError","param":null,"code":400}"#;
        assert_eq!(
            extract_error_message(body),
            "dimensions parameter is not supported for this model"
        );
    }

    #[test]
    fn test_fastapi_detail_format() {
        let body = r#"{"detail":"Validation failed"}"#;
        assert_eq!(extract_error_message(body), "Validation failed");
    }

    #[test]
    fn test_unknown_json_falls_back_to_body() {
        let body = r#"{"weird_shape":true}"#;
        assert_eq!(extract_error_message(body), body);
    }

    #[test]
    fn test_non_json_falls_back_to_body() {
        let body = "plain text error";
        assert_eq!(extract_error_message(body), body);
    }

    #[test]
    fn test_prefers_nested_error_over_flat_message() {
        // If both shapes are present, prefer the explicit error envelope.
        let body =
            r#"{"error":{"message":"from envelope"},"message":"from flat","type":"whatever"}"#;
        assert_eq!(extract_error_message(body), "from envelope");
    }
}
