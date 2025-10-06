use async_trait::async_trait;

/// Request to update model pricing and metadata
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct UpdateModelAdminRequest {
    pub input_cost_per_token: Option<i64>,
    pub output_cost_per_token: Option<i64>,
    pub model_display_name: Option<String>,
    pub model_description: Option<String>,
    pub model_icon: Option<String>,
    pub context_length: Option<i32>,
    pub verifiable: Option<bool>,
    pub is_active: Option<bool>,
}

/// Batch update request format - Map of model name to update data
pub type BatchUpdateModelAdminRequest = std::collections::HashMap<String, UpdateModelAdminRequest>;

/// Batch update response format - Map of model name to pricing data
pub type BatchUpdateModelAdminResponse = std::collections::HashMap<String, ModelPricing>;

/// Model pricing information (result after update)
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub model_display_name: String,
    pub model_description: String,
    pub model_icon: Option<String>,
    pub input_cost_per_token: i64,
    pub output_cost_per_token: i64,
    pub context_length: i32,
    pub verifiable: bool,
    pub is_active: bool,
}

/// Model pricing history entry
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct ModelPricingHistoryEntry {
    pub id: uuid::Uuid,
    pub model_id: uuid::Uuid,
    pub input_cost_per_token: i64,
    pub output_cost_per_token: i64,
    pub context_length: i32,
    pub model_display_name: String,
    pub model_description: String,
    pub effective_from: chrono::DateTime<chrono::Utc>,
    pub effective_until: Option<chrono::DateTime<chrono::Utc>>,
    pub changed_by: Option<String>,
    pub change_reason: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Request to update organization limits
/// All amounts use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct OrganizationLimitsUpdate {
    pub spend_limit: i64,
    pub changed_by: Option<String>,
    pub change_reason: Option<String>,
}

/// Organization limits (current active limits)
/// All amounts use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct OrganizationLimits {
    pub organization_id: uuid::Uuid,
    pub spend_limit: i64,
    pub effective_from: chrono::DateTime<chrono::Utc>,
}

/// Organization limits history entry
/// All amounts use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct OrganizationLimitsHistoryEntry {
    pub id: uuid::Uuid,
    pub organization_id: uuid::Uuid,
    pub spend_limit: i64,
    pub effective_from: chrono::DateTime<chrono::Utc>,
    pub effective_until: Option<chrono::DateTime<chrono::Utc>>,
    pub changed_by: Option<String>,
    pub change_reason: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("Model not found: {0}")]
    ModelNotFound(String),
    #[error("Organization not found: {0}")]
    OrganizationNotFound(String),
    #[error("Invalid pricing data: {0}")]
    InvalidPricing(String),
    #[error("Invalid limits data: {0}")]
    InvalidLimits(String),
    #[error("Unauthorized: {0}")]
    Unauthorized(String),
    #[error("Internal error: {0}")]
    InternalError(String),
}

/// Repository trait for admin operations on models and organizations
#[async_trait]
pub trait AdminRepository: Send + Sync {
    /// Upsert model pricing and metadata (insert or update)
    async fn upsert_model_pricing(
        &self,
        model_name: &str,
        request: UpdateModelAdminRequest,
    ) -> Result<ModelPricing, anyhow::Error>;

    /// Get pricing history for a model
    async fn get_pricing_history(
        &self,
        model_name: &str,
    ) -> Result<Vec<ModelPricingHistoryEntry>, anyhow::Error>;

    /// Update organization limits (creates new history entry, closes previous)
    async fn update_organization_limits(
        &self,
        organization_id: uuid::Uuid,
        limits: OrganizationLimitsUpdate,
    ) -> Result<OrganizationLimits, anyhow::Error>;

    /// Get current active limits for an organization
    async fn get_current_organization_limits(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<Option<OrganizationLimits>, anyhow::Error>;

    /// Get limits history for an organization
    async fn get_organization_limits_history(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<Vec<OrganizationLimitsHistoryEntry>, anyhow::Error>;
}

/// Admin service trait for managing platform configuration
#[async_trait]
pub trait AdminService: Send + Sync {
    /// Batch upsert models pricing and metadata (admin only)
    async fn batch_upsert_models(
        &self,
        models: BatchUpdateModelAdminRequest,
    ) -> Result<BatchUpdateModelAdminResponse, AdminError>;

    /// Get pricing history for a model (admin only)
    async fn get_pricing_history(
        &self,
        model_name: &str,
    ) -> Result<Vec<ModelPricingHistoryEntry>, AdminError>;

    /// Update organization limits (admin only)
    async fn update_organization_limits(
        &self,
        organization_id: uuid::Uuid,
        limits: OrganizationLimitsUpdate,
    ) -> Result<OrganizationLimits, AdminError>;

    /// Get limits history for an organization (admin only)
    async fn get_organization_limits_history(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<Vec<OrganizationLimitsHistoryEntry>, AdminError>;
}
