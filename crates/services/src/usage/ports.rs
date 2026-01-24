use crate::responses::models::ResponseId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Why an inference stream ended
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Stream completed normally (model emitted stop token)
    Completed,
    /// Hit max tokens limit
    Length,
    /// Content was filtered by safety systems
    ContentFilter,
    /// Client closed connection mid-stream
    ClientDisconnect,
    /// Provider returned an error during stream
    ProviderError,
    /// Request timed out
    Timeout,
    /// Tool/function call requested by model
    ToolCalls,
    /// Model decided to stop (explicit stop sequence)
    Stop,
    /// Request was rate limited (HTTP 429)
    RateLimited,
    /// Unmapped stop reason - stores the original value
    Other(String),
}

impl StopReason {
    /// Convert to database string representation
    pub fn as_str(&self) -> &str {
        match self {
            StopReason::Completed => "completed",
            StopReason::Length => "length",
            StopReason::ContentFilter => "content_filter",
            StopReason::ClientDisconnect => "client_disconnect",
            StopReason::ProviderError => "provider_error",
            StopReason::Timeout => "timeout",
            StopReason::ToolCalls => "tool_calls",
            StopReason::Stop => "stop",
            StopReason::RateLimited => "rate_limited",
            StopReason::Other(s) => s.as_str(),
        }
    }

    /// Parse from database string representation
    pub fn parse(s: &str) -> Self {
        match s {
            "completed" => StopReason::Completed,
            "length" => StopReason::Length,
            "content_filter" => StopReason::ContentFilter,
            "client_disconnect" => StopReason::ClientDisconnect,
            "provider_error" => StopReason::ProviderError,
            "timeout" => StopReason::Timeout,
            "tool_calls" => StopReason::ToolCalls,
            "stop" => StopReason::Stop,
            "rate_limited" => StopReason::RateLimited,
            other => StopReason::Other(other.to_string()),
        }
    }

    /// Parse from OpenAI-compatible finish_reason field (string version)
    pub fn from_finish_reason(reason: &str) -> Self {
        match reason {
            "stop" => StopReason::Stop,
            "length" => StopReason::Length,
            "content_filter" => StopReason::ContentFilter,
            "tool_calls" | "function_call" => StopReason::ToolCalls,
            other => StopReason::Other(other.to_string()),
        }
    }

    /// Convert from inference provider's FinishReason enum
    pub fn from_provider_finish_reason(reason: &inference_providers::FinishReason) -> Self {
        match reason {
            inference_providers::FinishReason::Stop => StopReason::Stop,
            inference_providers::FinishReason::Length => StopReason::Length,
            inference_providers::FinishReason::ContentFilter => StopReason::ContentFilter,
            inference_providers::FinishReason::ToolCalls => StopReason::ToolCalls,
        }
    }

    /// Map from inference provider CompletionError to StopReason
    pub fn from_completion_error(err: &inference_providers::CompletionError) -> Self {
        match err {
            inference_providers::CompletionError::HttpError { status_code, .. } => {
                match status_code {
                    408 => StopReason::Timeout,
                    429 => StopReason::RateLimited,
                    500..=599 => StopReason::ProviderError,
                    _ => StopReason::ProviderError,
                }
            }
            inference_providers::CompletionError::CompletionError(msg) => {
                let msg_lower = msg.to_lowercase();
                if msg_lower.contains("timeout") {
                    StopReason::Timeout
                } else if msg_lower.contains("rate limit") || msg_lower.contains("too many") {
                    StopReason::RateLimited
                } else {
                    StopReason::ProviderError
                }
            }
            inference_providers::CompletionError::InvalidResponse(_) => StopReason::ProviderError,
            inference_providers::CompletionError::Unknown(_) => StopReason::ProviderError,
        }
    }
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ============================================
// Service Traits
// ============================================

#[async_trait::async_trait]
pub trait UsageServiceTrait: Send + Sync {
    /// Calculate cost for a given model and token usage
    async fn calculate_cost(
        &self,
        model_id: &str,
        input_tokens: i32,
        output_tokens: i32,
    ) -> Result<CostBreakdown, UsageError>;

    /// Record usage after an API call completes
    async fn record_usage(&self, request: RecordUsageServiceRequest) -> Result<(), UsageError>;

    /// Check if organization can make an API call (pre-flight check)
    async fn check_can_use(&self, organization_id: Uuid) -> Result<UsageCheckResult, UsageError>;

    /// Get current balance for an organization
    async fn get_balance(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<OrganizationBalanceInfo>, UsageError>;

    /// Get usage history for an organization
    /// Returns a tuple of (entries, total_count)
    async fn get_usage_history(
        &self,
        organization_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError>;

    /// Get current spending limit for an organization
    async fn get_limit(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<OrganizationLimit>, UsageError>;

    /// Get usage history for a specific API key
    /// Returns a tuple of (entries, total_count)
    async fn get_usage_history_by_api_key(
        &self,
        api_key_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError>;

    /// Get usage history for a specific API key with permission checking
    /// This method verifies the user has access to the workspace and that the API key exists
    /// Returns a tuple of (entries, total_count)
    async fn get_api_key_usage_history_with_permissions(
        &self,
        workspace_id: Uuid,
        api_key_id: Uuid,
        user_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError>;

    /// Get costs by inference IDs (for HuggingFace billing integration)
    /// Returns costs for each inference_id that was found and belongs to the organization
    async fn get_costs_by_inference_ids(
        &self,
        organization_id: Uuid,
        inference_ids: Vec<Uuid>,
    ) -> Result<Vec<InferenceCost>, UsageError>;
}

// ============================================
// Repository Traits
// ============================================

#[async_trait::async_trait]
pub trait UsageRepository: Send + Sync {
    /// Record usage and update balance atomically
    async fn record_usage(&self, request: RecordUsageDbRequest) -> anyhow::Result<UsageLogEntry>;

    /// Get current balance for an organization
    async fn get_balance(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationBalanceInfo>>;

    /// Get usage history for an organization
    /// Returns a tuple of (entries, total_count)
    async fn get_usage_history(
        &self,
        organization_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)>;

    /// Get usage history for a specific API key
    /// Returns a tuple of (entries, total_count)
    async fn get_usage_history_by_api_key(
        &self,
        api_key_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)>;

    /// Get total spend for a specific API key
    async fn get_api_key_spend(&self, api_key_id: Uuid) -> anyhow::Result<i64>;

    /// Get costs by inference IDs (for HuggingFace billing integration)
    /// Returns costs for each inference_id that was found and belongs to the organization
    async fn get_costs_by_inference_ids(
        &self,
        organization_id: Uuid,
        inference_ids: Vec<Uuid>,
    ) -> anyhow::Result<Vec<InferenceCost>>;

    /// Get the stop reason for a specific response ID
    /// Used to check if a response was stopped due to client disconnect
    async fn get_stop_reason_by_response_id(
        &self,
        response_id: Uuid,
    ) -> anyhow::Result<Option<StopReason>>;

    /// Get the stop reason for a specific provider request ID (e.g., chatcmpl-xxx)
    /// Used to check if a chat completion was stopped due to client disconnect
    async fn get_stop_reason_by_provider_request_id(
        &self,
        provider_request_id: &str,
    ) -> anyhow::Result<Option<StopReason>>;
}

#[async_trait::async_trait]
pub trait ModelRepository: Send + Sync {
    /// Get model by name
    async fn get_model_by_name(&self, model_name: &str) -> anyhow::Result<Option<ModelPricing>>;

    /// Get model by UUID
    async fn get_model_by_id(&self, model_id: Uuid) -> anyhow::Result<Option<ModelPricing>>;
}

#[async_trait::async_trait]
pub trait OrganizationLimitsRepository: Send + Sync {
    /// Get current limits for an organization
    async fn get_current_limits(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationLimit>>;
}

// ============================================
// Service Data Structures
// ============================================

/// Request to record usage (service layer)
#[derive(Debug, Clone)]
pub struct RecordUsageServiceRequest {
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub model_id: Uuid,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub inference_type: String, // 'chat_completion', 'chat_completion_stream', 'image_generation', etc.
    /// Time to first token in milliseconds
    pub ttft_ms: Option<i32>,
    /// Average inter-token latency in milliseconds
    pub avg_itl_ms: Option<f64>,
    /// Inference UUID (hashed from provider_request_id)
    pub inference_id: Option<Uuid>,
    /// Raw request ID from the inference provider (e.g., vLLM chat_id)
    pub provider_request_id: Option<String>,
    /// Why the inference stream ended
    pub stop_reason: Option<StopReason>,
    /// Response ID for Response API calls (FK to responses table)
    pub response_id: Option<ResponseId>,
    /// Number of images generated (for image generation requests)
    pub image_count: Option<i32>,
}

/// Request to record usage (database layer)
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct RecordUsageDbRequest {
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub model_id: Uuid,
    pub model_name: String, // Denormalized canonical model name
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub input_cost: i64,
    pub output_cost: i64,
    pub total_cost: i64,
    pub inference_type: String,
    /// Time to first token in milliseconds
    pub ttft_ms: Option<i32>,
    /// Average inter-token latency in milliseconds
    pub avg_itl_ms: Option<f64>,
    /// Inference UUID (hashed from provider_request_id)
    pub inference_id: Option<Uuid>,
    /// Raw request ID from the inference provider (e.g., vLLM chat_id)
    pub provider_request_id: Option<String>,
    /// Why the inference stream ended
    pub stop_reason: Option<StopReason>,
    /// Response ID for Response API calls (FK to responses table)
    pub response_id: Option<ResponseId>,
    /// Number of images generated (for image generation requests)
    pub image_count: Option<i32>,
}

/// Model pricing information
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub id: Uuid,
    pub model_name: String, // Canonical model name
    pub input_cost_per_token: i64,
    pub output_cost_per_token: i64,
    pub cost_per_image: i64,
}

/// Organization spending limit
/// All amounts use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct OrganizationLimit {
    pub spend_limit: i64,
}

/// Cost breakdown for a request
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct CostBreakdown {
    pub input_cost: i64,
    pub output_cost: i64,
    pub total_cost: i64,
}

/// Inference cost for billing (HuggingFace compatible)
/// Cost is in nano-USD (10^-9 USD)
#[derive(Debug, Clone)]
pub struct InferenceCost {
    pub inference_id: Uuid,
    pub cost_nano_usd: i64,
}

/// Result of checking if organization can use credits
/// All amounts use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub enum UsageCheckResult {
    Allowed { remaining: i64 },
    LimitExceeded { spent: i64, limit: i64 },
    NoCredits,  // No credits available - must purchase credits
    NoLimitSet, // No spending limit configured - must set limit
}

/// Organization balance information
/// All amounts use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct OrganizationBalanceInfo {
    pub organization_id: Uuid,
    pub total_spent: i64,
    pub last_usage_at: Option<DateTime<Utc>>,
    pub total_requests: i64,
    pub total_tokens: i64,
    pub updated_at: DateTime<Utc>,
}

/// Usage log entry
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct UsageLogEntry {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub model_id: Uuid,
    pub model: String, // Canonical model name from models table
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub total_tokens: i32,
    pub input_cost: i64,
    pub output_cost: i64,
    pub total_cost: i64,
    pub inference_type: String,
    pub created_at: DateTime<Utc>,
    /// Time to first token in milliseconds
    pub ttft_ms: Option<i32>,
    /// Average inter-token latency in milliseconds
    pub avg_itl_ms: Option<f64>,
    /// Inference UUID (hashed from provider_request_id)
    pub inference_id: Option<Uuid>,
    /// Raw request ID from the inference provider (e.g., vLLM chat_id)
    pub provider_request_id: Option<String>,
    /// Why the inference stream ended
    pub stop_reason: Option<StopReason>,
    /// Response ID for Response API calls (FK to responses table)
    pub response_id: Option<ResponseId>,
    /// Number of images generated (for image generation requests)
    pub image_count: Option<i32>,
}

// ============================================
// Error Types
// ============================================

#[derive(Debug, Clone)]
pub enum UsageError {
    ModelNotFound(String),
    InternalError(String),
    LimitExceeded(String),
    Unauthorized(String),
    NotFound(String),
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UsageError::ModelNotFound(msg) => write!(f, "Model not found: {msg}"),
            UsageError::InternalError(msg) => write!(f, "Internal error: {msg}"),
            UsageError::LimitExceeded(msg) => write!(f, "Limit exceeded: {msg}"),
            UsageError::Unauthorized(msg) => write!(f, "Unauthorized: {msg}"),
            UsageError::NotFound(msg) => write!(f, "Not found: {msg}"),
        }
    }
}

impl std::error::Error for UsageError {}
