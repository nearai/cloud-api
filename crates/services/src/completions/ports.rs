use crate::responses::models::ResponseId;
use crate::UserId;
use async_trait::async_trait;
use inference_providers::StreamingResult;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Default concurrent request limit per organization per model
pub const DEFAULT_CONCURRENT_LIMIT: u32 = 64;

// Domain types defined directly here (following dependency inversion)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionId(Uuid);

impl From<Uuid> for CompletionId {
    fn from(uuid: Uuid) -> Self {
        CompletionId(uuid)
    }
}

impl std::fmt::Display for CompletionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "comp_{}", self.0)
    }
}

// Error types
#[derive(Debug, thiserror::Error)]
pub enum CompletionError {
    #[error("Invalid model: {0}")]
    InvalidModel(String),

    #[error("Rate limit exceeded")]
    RateLimitExceeded,

    #[error("Invalid parameters: {0}")]
    InvalidParams(String),

    #[error("Provider error: {0}")]
    ProviderError(String),

    #[error("Service overloaded: {0}")]
    ServiceOverloaded(String),

    #[error("Internal error: {0}")]
    InternalError(String),
}

// Request/Response models
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<CompletionMessage>,
    pub max_tokens: Option<i64>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop: Option<Vec<String>>,
    pub stream: Option<bool>,
    pub n: Option<i64>,
    pub user_id: UserId,    // For provider user field
    pub api_key_id: String, // For usage tracking (ID only, no name)
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub metadata: Option<serde_json::Value>,
    /// Whether to store the output (required for metadata to be sent to OpenAI)
    pub store: Option<bool>,
    pub body_hash: String,
    /// Response ID when called from Responses API (for usage tracking FK)
    pub response_id: Option<ResponseId>,

    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

/// Tool call information for completion messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionToolCall {
    /// Tool call ID (e.g., "toolu_xxx" for Anthropic, "call_xxx" for OpenAI)
    pub id: String,
    /// Tool name (e.g., "web_search")
    pub name: String,
    /// JSON-encoded arguments for the tool
    pub arguments: String,
    /// Thought signature for Gemini 3 models (internal use only, not exposed to clients)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionMessage {
    pub role: String,
    pub content: String,
    /// Tool call ID - required for tool role messages to match with assistant's tool_calls
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool calls made by the assistant - required for assistant messages that invoke tools
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<CompletionToolCall>>,
    /// Multimodal content (images, etc.) for supporting image analysis and other multimodal tasks
    /// Serialized as JSON array of content objects compatible with OpenAI format
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multimodal_content: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub capabilities: ModelCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub chat: bool,
    pub completion: bool,
    pub embeddings: bool,
}

// Port/Trait definitions (no implementations!)

/// Repository trait for fetching organization concurrent limits
/// Used by CompletionService to look up per-org rate limits
#[async_trait]
pub trait OrganizationConcurrentLimitRepository: Send + Sync {
    /// Get the concurrent request limit for an organization
    /// Returns None if no custom limit is set (use default)
    async fn get_concurrent_limit(&self, org_id: Uuid) -> Result<Option<u32>, anyhow::Error>;
}

#[async_trait]
pub trait CompletionServiceTrait: Send + Sync {
    /// Create a streaming completion
    async fn create_chat_completion_stream(
        &self,
        request: CompletionRequest,
    ) -> Result<StreamingResult, CompletionError>;

    async fn create_chat_completion(
        &self,
        request: CompletionRequest,
    ) -> Result<inference_providers::ChatCompletionResponseWithBytes, CompletionError>;

    /// Get reference to inference provider pool for image operations
    fn get_inference_provider_pool(
        &self,
    ) -> std::sync::Arc<crate::inference_provider_pool::InferenceProviderPool>;

    /// Get model details by name (canonical name or alias)
    /// Returns None if the model is not found or not active
    async fn get_model(
        &self,
        model_name: &str,
    ) -> Result<Option<crate::models::ModelWithPricing>, anyhow::Error>;

    /// Execute an audio transcription request with concurrent request limiting
    async fn audio_transcription(
        &self,
        organization_id: uuid::Uuid,
        model_id: uuid::Uuid,
        model_name: &str,
        params: inference_providers::AudioTranscriptionParams,
        request_hash: String,
    ) -> Result<inference_providers::AudioTranscriptionResponse, CompletionError>;

    /// Execute a rerank request with proper concurrent request limiting.
    ///
    /// Each organization has a per-model concurrent request limit (default: 64).
    /// This method acquires a concurrent slot before calling the inference provider.
    /// If the limit is exceeded, returns CompletionError::RateLimitExceeded (429 HTTP status).
    /// Slots are automatically released after the provider call (success or error).
    async fn try_rerank(
        &self,
        organization_id: Uuid,
        model_id: Uuid,
        model_name: &str,
        params: inference_providers::RerankParams,
    ) -> Result<inference_providers::RerankResponse, CompletionError>;

    /// Execute a score request with proper concurrent request limiting.
    ///
    /// Each organization has a per-model concurrent request limit (default: 64).
    /// This method acquires a concurrent slot before calling the inference provider.
    /// If the limit is exceeded, returns CompletionError::RateLimitExceeded (429 HTTP status).
    /// Slots are automatically released after the provider call (success or error).
    async fn try_score(
        &self,
        organization_id: Uuid,
        model_id: Uuid,
        model_name: &str,
        request_hash: String,
        params: inference_providers::ScoreParams,
    ) -> Result<inference_providers::ScoreResponse, CompletionError>;
}
