use chrono::{DateTime, Utc};
use uuid::Uuid;

// ============================================
// Service Traits
// ============================================

#[async_trait::async_trait]
pub trait UsageService: Send + Sync {
    /// Calculate cost for a given model and token usage
    async fn calculate_cost(
        &self,
        model_id: &str,
        input_tokens: u32,
        output_tokens: u32,
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
    async fn get_usage_history(
        &self,
        organization_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<UsageLogEntry>, UsageError>;
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
    async fn get_usage_history(
        &self,
        organization_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> anyhow::Result<Vec<UsageLogEntry>>;
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
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub request_type: String, // 'chat_completion', 'text_completion', 'response'
}

/// Request to record usage (database layer)
#[derive(Debug, Clone)]
pub struct RecordUsageDbRequest {
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub response_id: Option<Uuid>,
    pub model_id: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub input_cost_amount: i64,
    pub input_cost_scale: i32,
    pub input_cost_currency: String,
    pub output_cost_amount: i64,
    pub output_cost_scale: i32,
    pub output_cost_currency: String,
    pub total_cost_amount: i64,
    pub total_cost_scale: i32,
    pub total_cost_currency: String,
    pub request_type: String,
}

/// Model pricing information
#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input_cost_amount: i64,
    pub input_cost_scale: i32,
    pub input_cost_currency: String,
    pub output_cost_amount: i64,
    pub output_cost_scale: i32,
    pub output_cost_currency: String,
}

/// Organization spending limit
#[derive(Debug, Clone)]
pub struct OrganizationLimit {
    pub spend_limit_amount: i64,
    pub spend_limit_scale: i32,
    pub spend_limit_currency: String,
}

/// Cost breakdown for a request
#[derive(Debug, Clone)]
pub struct CostBreakdown {
    pub input_cost_amount: i64,
    pub input_cost_scale: i32,
    pub input_cost_currency: String,
    pub output_cost_amount: i64,
    pub output_cost_scale: i32,
    pub output_cost_currency: String,
    pub total_cost_amount: i64,
    pub total_cost_scale: i32,
    pub total_cost_currency: String,
}

/// Result of checking if organization can use credits
#[derive(Debug, Clone)]
pub enum UsageCheckResult {
    Allowed {
        remaining_amount: i64,
        remaining_scale: i32,
        remaining_currency: String,
    },
    LimitExceeded {
        spent_amount: i64,
        spent_scale: i32,
        spent_currency: String,
        limit_amount: i64,
        limit_scale: i32,
        limit_currency: String,
    },
    NoCredits,  // No credits available - must purchase credits
    NoLimitSet, // No spending limit configured - must set limit
    CurrencyMismatch {
        spent_currency: String,
        limit_currency: String,
    },
}

/// Organization balance information
#[derive(Debug, Clone)]
pub struct OrganizationBalanceInfo {
    pub organization_id: Uuid,
    pub total_spent_amount: i64,
    pub total_spent_scale: i32,
    pub total_spent_currency: String,
    pub last_usage_at: Option<DateTime<Utc>>,
    pub total_requests: i64,
    pub total_tokens: i64,
    pub updated_at: DateTime<Utc>,
}

/// Usage log entry
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
    pub input_cost_amount: i64,
    pub input_cost_scale: i32,
    pub input_cost_currency: String,
    pub output_cost_amount: i64,
    pub output_cost_scale: i32,
    pub output_cost_currency: String,
    pub total_cost_amount: i64,
    pub total_cost_scale: i32,
    pub total_cost_currency: String,
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
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UsageError::ModelNotFound(msg) => write!(f, "Model not found: {}", msg),
            UsageError::InternalError(msg) => write!(f, "Internal error: {}", msg),
            UsageError::LimitExceeded(msg) => write!(f, "Limit exceeded: {}", msg),
        }
    }
}

impl std::error::Error for UsageError {}
