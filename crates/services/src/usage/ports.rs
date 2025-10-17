use chrono::{DateTime, Utc};
use uuid::Uuid;

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
}

#[async_trait::async_trait]
pub trait ModelRepository: Send + Sync {
    /// Get model by name
    async fn get_model_by_name(&self, model_name: &str) -> anyhow::Result<Option<ModelPricing>>;
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
    pub response_id: Option<Uuid>,
    pub model_id: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub request_type: String, // 'chat_completion', 'text_completion', 'response'
}

/// Request to record usage (database layer)
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct RecordUsageDbRequest {
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub response_id: Option<Uuid>,
    pub model_id: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub input_cost: i64,
    pub output_cost: i64,
    pub total_cost: i64,
    pub request_type: String,
}

/// Model pricing information
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input_cost_per_token: i64,
    pub output_cost_per_token: i64,
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
    pub response_id: Option<Uuid>,
    pub model_id: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub total_tokens: i32,
    pub input_cost: i64,
    pub output_cost: i64,
    pub total_cost: i64,
    pub request_type: String,
    pub created_at: DateTime<Utc>,
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
