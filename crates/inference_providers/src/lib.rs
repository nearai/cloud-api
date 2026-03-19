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

pub mod chunk_builder;
pub mod external;
pub mod mock;
pub mod models;
pub mod sse_parser;
pub mod vllm;

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
    ImageGenerationResponse, ImageGenerationResponseWithBytes, MessageRole, ModelInfo, RerankError,
    RerankParams, RerankResponse, RerankResult, RerankUsage, ScoreError, ScoreParams,
    ScoreResponse, ScoreResult, ScoreUsage, StreamChunk, StreamOptions, TokenUsage, ToolChoice,
    ToolDefinition, TranscriptionSegment, TranscriptionWord,
};
pub use sse_parser::{new_sse_parser, BufferedSSEParser, SSEEvent, SSEEventParser, SSEParser};
pub use vllm::{VLlmConfig, VLlmProvider};

// Chunk builder for external provider parsers
pub use chunk_builder::ChunkContext;

// External provider exports
pub use external::{
    AnthropicBackend, ExternalProvider, ExternalProviderConfig, GeminiBackend,
    OpenAiCompatibleBackend, ProviderConfig,
};

/// Try to extract a human-readable error message from a JSON error response body.
///
/// Supports common formats:
///   - OpenAI/Anthropic: `{"error": {"message": "..."}}`
///   - vLLM/FastAPI: `{"detail": "..."}`
///   - SGLang/vLLM OpenAI-compat: `{"message": "..."}`
///   - Falls back to the raw body if neither matches
///
/// The extracted message is sanitized to strip user data from validation error
/// details (the `'input'` and `'ctx'` fields in Python-formatted validation dicts),
/// preventing conversation content from leaking in error responses.
pub fn extract_error_message(body: &str) -> String {
    let raw_message = if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        // OpenAI/Anthropic format: {"error": {"message": "..."}}
        if let Some(msg) = json
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            msg.to_string()
        }
        // vLLM/FastAPI format: {"detail": "..."}
        else if let Some(detail) = json.get("detail").and_then(|d| d.as_str()) {
            detail.to_string()
        }
        // SGLang/vLLM OpenAI-compat format: {"message": "..."}
        else if let Some(msg) = json.get("message").and_then(|m| m.as_str()) {
            msg.to_string()
        } else {
            body.to_string()
        }
    } else {
        body.to_string()
    };

    // Sanitize to prevent leaking user conversation content in validation error details
    sanitize_provider_error(&raw_message)
}

/// Regex to extract 'type' values from Python-formatted validation error dicts.
static VALIDATION_TYPE_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"'type':\s*'([^']+)'").unwrap());

/// Regex to extract 'msg' values from Python-formatted validation error dicts.
/// Handles both single-quoted and double-quoted strings.
static VALIDATION_MSG_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r#"'msg':\s*(?:'([^']*)'|"([^"]*)")"#).unwrap());

/// Sanitize provider error messages to prevent leaking user conversation content.
///
/// Backend validation errors (from SGLang/vLLM) include `'input'` and `'ctx'` fields
/// containing the original request data, which may include user messages, AI responses,
/// and other sensitive conversation content. This function strips those fields while
/// preserving useful error type and message information.
fn sanitize_provider_error(message: &str) -> String {
    // Only sanitize if the message contains user input data in validation errors
    if !message.contains("'input':") {
        return message.to_string();
    }

    let mut result = Vec::new();

    for line in message.lines() {
        let trimmed = line.trim();

        // Skip stack traces and HTTP method lines (also leak internal paths)
        if trimmed.starts_with("File \"")
            || trimmed.starts_with("POST ")
            || trimmed.starts_with("GET ")
        {
            continue;
        }

        // For validation error dict lines containing 'input', extract only type and msg
        if trimmed.starts_with('{') && trimmed.contains("'input':") {
            let error_type = VALIDATION_TYPE_RE
                .captures(trimmed)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str());
            let error_msg = VALIDATION_MSG_RE
                .captures(trimmed)
                .and_then(|c| c.get(1).or_else(|| c.get(2)))
                .map(|m| m.as_str());

            match (error_type, error_msg) {
                (Some(t), Some(m)) => result.push(format!("  {}: {}", t, m)),
                (Some(t), None) => result.push(format!("  {}", t)),
                _ => result.push("  (validation error)".to_string()),
            }
        } else {
            result.push(line.to_string());
        }
    }

    result.join("\n")
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
    async fn embeddings_raw(&self, body: bytes::Bytes) -> Result<bytes::Bytes, EmbeddingError>;

    async fn get_signature(
        &self,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<ChatSignature, CompletionError>;

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
mod tests {
    use super::*;

    #[test]
    fn test_extract_error_message_openai_format() {
        let body = r#"{"error": {"message": "Invalid model", "type": "invalid_request_error"}}"#;
        assert_eq!(extract_error_message(body), "Invalid model");
    }

    #[test]
    fn test_extract_error_message_fastapi_format() {
        let body = r#"{"detail": "Not found"}"#;
        assert_eq!(extract_error_message(body), "Not found");
    }

    #[test]
    fn test_extract_error_message_sglang_format() {
        let body = r#"{"object": "error", "message": "Context length exceeded"}"#;
        assert_eq!(extract_error_message(body), "Context length exceeded");
    }

    #[test]
    fn test_extract_error_message_raw_fallback() {
        let body = "Something went wrong";
        assert_eq!(extract_error_message(body), "Something went wrong");
    }

    #[test]
    fn test_sanitize_strips_input_from_validation_errors() {
        let message = concat!(
            "2 validation errors:\n",
            "  {'type': 'value_error', 'loc': ('body', 'messages', 1), 'msg': \"Value error, invalid role\", 'input': 'user', 'ctx': {'error': ValueError(\"bad\")}}\n",
            "  {'type': 'string_type', 'loc': ('body', 'messages', 1, 'content'), 'msg': 'Input should be a valid string', 'input': [{'text': 'secret user conversation content', 'type': 'custom'}]}"
        );
        let result = sanitize_provider_error(message);
        assert!(!result.contains("secret user conversation"));
        assert!(!result.contains("'input':"));
        assert!(result.contains("2 validation errors:"));
        assert!(result.contains("value_error: Value error, invalid role"));
        assert!(result.contains("string_type: Input should be a valid string"));
    }

    #[test]
    fn test_sanitize_strips_stack_traces() {
        let message = concat!(
            "1 validation errors:\n",
            "  {'type': 'value_error', 'msg': 'bad', 'input': 'x'}\n",
            "  File \"/sgl-workspace/sglang/python/sglang/srt/entrypoints/http_server.py\", line 1324\n",
            "    POST /v1/chat/completions some data"
        );
        let result = sanitize_provider_error(message);
        assert!(!result.contains("sgl-workspace"));
        assert!(!result.contains("POST /v1/chat"));
    }

    #[test]
    fn test_sanitize_preserves_non_validation_errors() {
        let message = "Context length exceeded: 32768 tokens requested, 16384 max";
        assert_eq!(sanitize_provider_error(message), message);
    }

    #[test]
    fn test_extract_sglang_validation_error_full_body() {
        // Simulates the actual SGLang response format — extract_error_message should
        // parse the JSON, extract the message field, and sanitize it
        let body = r#"{"object":"error","message":"1 validation errors:\n  {'type': 'value_error', 'loc': ('body',), 'msg': 'bad request', 'input': 'sensitive user data', 'ctx': {'error': ValueError('details')}}"}"#;
        let result = extract_error_message(body);
        assert!(!result.contains("sensitive user data"));
        assert!(result.contains("1 validation errors:"));
        assert!(result.contains("value_error: bad request"));
    }
}
