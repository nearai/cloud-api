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

pub mod models;
mod sse_parser;
pub mod vllm;

use std::pin::Pin;

use async_trait::async_trait;
use futures_core::Stream;
use models::*;
use tokio_stream::StreamExt;

// Re-export commonly used types for convenience
pub use models::{
    AttestationReportParams, ChatCompletionParams, ChatDelta, ChatMessage, ChatSignature,
    CompletionError, CompletionParams, FinishReason, MessageRole, ModelInfo, NvidiaPayload,
    StreamChunk, StreamOptions, TokenUsage, VllmAttestationReport,
};
pub use vllm::{VLlmConfig, VLlmProvider};

/// Type alias for streaming completion results
///
/// This represents a stream of chunks where each chunk can either be:
/// - `Ok(StreamChunk)` - A successful chunk with partial content
/// - `Err(CompletionError)` - An error that occurred during streaming
pub type StreamingResult = Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>;

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

    /// Performs a streaming text completion request
    ///
    /// Returns a stream of `StreamChunk` objects for incremental text generation.
    /// Similar to chat completion but for raw text prompts rather than conversations.
    async fn text_completion_stream(
        &self,
        params: CompletionParams,
    ) -> Result<StreamingResult, CompletionError>;

    async fn get_signature(&self, chat_id: &str) -> Result<ChatSignature, CompletionError>;

    async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
    ) -> Result<Vec<VllmAttestationReport>, CompletionError>;
}
