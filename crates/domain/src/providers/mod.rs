pub mod vllm;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use crate::{
    errors::CompletionError,
    models::*,
};

// ============================================================================
// Provider Traits
// ============================================================================

/// Trait for LLM providers that support both streaming and non-streaming completions
#[async_trait]
pub trait CompletionProvider: Send + Sync {
    /// Get the name of this provider
    fn name(&self) -> &str;
    
    /// Get available models from this provider
    async fn get_models(&self) -> Result<Vec<ModelInfo>, CompletionError>;
    
    /// Check if this provider supports a given model
    fn supports_model(&self, model_id: &str) -> bool;
    
    /// Perform a chat completion (non-streaming)
    async fn chat_completion(
        &self,
        params: ChatCompletionParams,
    ) -> Result<ChatCompletionResult, CompletionError>;
    
    /// Perform a chat completion with streaming
    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError>;
    
    /// Perform a text completion (non-streaming)
    async fn text_completion(
        &self,
        params: CompletionParams,
    ) -> Result<CompletionResult, CompletionError>;
    
    /// Perform a text completion with streaming
    async fn text_completion_stream(
        &self,
        params: CompletionParams,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError>;
}

// ============================================================================
// Streaming Models
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<StreamChoice>,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChoice {
    pub index: u32,
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delta {
    pub role: Option<String>,
    pub content: Option<String>,
}

// ============================================================================
// Model Discovery
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub object: String,
    pub created: Option<u64>,
    pub owned_by: Option<String>,
    pub provider: String,  // Which provider serves this model
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelInfo>,
}


