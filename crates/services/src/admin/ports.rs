use async_trait::async_trait;

/// Request to update model pricing and metadata
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct UpdateModelAdminRequest {
    pub input_cost_per_token: Option<i64>,
    pub output_cost_per_token: Option<i64>,
    pub cost_per_image: Option<i64>,
    pub model_display_name: Option<String>,
    pub model_description: Option<String>,
    pub model_icon: Option<String>,
    pub context_length: Option<i32>,
    pub verifiable: Option<bool>,
    pub is_active: Option<bool>,
    pub aliases: Option<Vec<String>>,
    pub owned_by: Option<String>,
    // Provider configuration
    pub provider_type: Option<String>,
    pub provider_config: Option<serde_json::Value>,
    pub attestation_supported: Option<bool>,
    // Architecture/modalities
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
    // User audit tracking for history
    pub change_reason: Option<String>,
    pub changed_by_user_id: Option<uuid::Uuid>,
    pub changed_by_user_email: Option<String>,
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
    pub cost_per_image: i64,
    pub context_length: i32,
    pub verifiable: bool,
    pub is_active: bool,
    pub aliases: Vec<String>,
    pub owned_by: String,
    // Provider configuration
    /// Provider type: "vllm" (TEE-enabled) or "external" (3rd party)
    pub provider_type: String,
    /// JSON config for external providers (backend, base_url, etc.)
    pub provider_config: Option<serde_json::Value>,
    /// Whether this model supports TEE attestation
    pub attestation_supported: bool,
    // Architecture/modalities
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
}

/// Model history entry - includes pricing, context length, and other model attributes
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct ModelHistoryEntry {
    pub id: uuid::Uuid,
    pub model_id: uuid::Uuid,
    pub input_cost_per_token: i64,
    pub output_cost_per_token: i64,
    pub cost_per_image: i64,
    pub context_length: i32,
    pub model_name: String,
    pub model_display_name: String,
    pub model_description: String,
    pub model_icon: Option<String>,
    pub verifiable: bool,
    pub is_active: bool,
    pub owned_by: String,
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
    pub effective_from: chrono::DateTime<chrono::Utc>,
    pub effective_until: Option<chrono::DateTime<chrono::Utc>>,
    pub changed_by_user_id: Option<uuid::Uuid>,
    pub changed_by_user_email: Option<String>,
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
    pub changed_by_user_id: Option<uuid::Uuid>, // The authenticated user ID who made the change
    pub changed_by_user_email: Option<String>, // The email of the authenticated user who made the change
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
    pub changed_by_user_id: Option<uuid::Uuid>, // The authenticated user ID who made the change
    pub changed_by_user_email: Option<String>, // The email of the authenticated user who made the change
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// User information for admin endpoints
#[derive(Debug, Clone)]
pub struct UserInfo {
    pub id: uuid::Uuid,
    pub email: String,
    pub username: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_login_at: Option<chrono::DateTime<chrono::Utc>>,
    pub is_active: bool,
}

/// Organization information for user listing (earliest organization with spend limit and usage)
#[derive(Debug, Clone)]
pub struct UserOrganizationInfo {
    pub id: uuid::Uuid,
    pub name: String,
    pub description: Option<String>,
    pub spend_limit: Option<i64>, // Amount in nano-dollars (scale 9)
    pub total_spent: Option<i64>, // Amount in nano-dollars (scale 9)
    pub total_requests: Option<i64>,
    pub total_tokens: Option<i64>,
}

/// Model information for admin listing (includes is_active status)
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct AdminModelInfo {
    pub id: uuid::Uuid,
    pub model_name: String,
    pub model_display_name: String,
    pub model_description: String,
    pub model_icon: Option<String>,
    pub input_cost_per_token: i64,
    pub output_cost_per_token: i64,
    pub cost_per_image: i64,
    pub context_length: i32,
    pub verifiable: bool,
    pub is_active: bool,
    pub owned_by: String,
    pub aliases: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    // Provider configuration
    /// Provider type: "vllm" (TEE-enabled) or "external" (3rd party)
    pub provider_type: String,
    /// JSON config for external providers (backend, base_url, etc.)
    pub provider_config: Option<serde_json::Value>,
    /// Whether this model supports TEE attestation
    pub attestation_supported: bool,
    // Architecture/modalities
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
}

/// Organization information for admin listing (includes spend limit and usage)
#[derive(Debug, Clone)]
pub struct AdminOrganizationInfo {
    pub id: uuid::Uuid,
    pub name: String,
    pub description: Option<String>,
    pub spend_limit: Option<i64>, // Amount in nano-dollars (scale 9)
    pub total_spent: Option<i64>, // Amount in nano-dollars (scale 9)
    pub total_requests: Option<i64>,
    pub total_tokens: Option<i64>,
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

    /// Get complete history for a model with pagination (includes pricing and other attributes)
    async fn get_model_history(
        &self,
        model_name: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ModelHistoryEntry>, i64), anyhow::Error>;

    /// Soft delete a model by setting is_active to false
    async fn soft_delete_model(
        &self,
        model_name: &str,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<bool, anyhow::Error>;

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

    /// Count limits history for an organization
    async fn count_organization_limits_history(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<i64, anyhow::Error>;

    /// Get limits history for an organization
    async fn get_organization_limits_history(
        &self,
        organization_id: uuid::Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OrganizationLimitsHistoryEntry>, anyhow::Error>;

    /// List all users with pagination (admin only)
    async fn list_users(&self, limit: i64, offset: i64) -> Result<Vec<UserInfo>, anyhow::Error>;

    /// List all users with their earliest organization and spend limit (admin only)
    /// If search_by_name is provided, filters users by organization name (case-insensitive partial match)
    /// Returns a tuple of (users, total_count) where total_count is the count of filtered users
    async fn list_users_with_organizations(
        &self,
        limit: i64,
        offset: i64,
        search_by_name: Option<String>,
    ) -> Result<(Vec<(UserInfo, Option<UserOrganizationInfo>)>, i64), anyhow::Error>;

    /// Get the count of active users (admin only)
    async fn get_active_user_count(&self) -> Result<i64, anyhow::Error>;

    /// List all models with pagination (admin only)
    /// If include_inactive is true, includes disabled models
    async fn list_models(
        &self,
        include_inactive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminModelInfo>, i64), anyhow::Error>;

    /// Update organization concurrent request limit
    /// Set to None to use the default limit
    async fn update_organization_concurrent_limit(
        &self,
        organization_id: uuid::Uuid,
        concurrent_limit: Option<u32>,
    ) -> Result<(), anyhow::Error>;

    /// Get organization concurrent request limit
    /// Returns None if using default
    async fn get_organization_concurrent_limit(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<Option<u32>, anyhow::Error>;

    /// List all organizations with pagination (admin only)
    async fn list_all_organizations(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AdminOrganizationInfo>, anyhow::Error>;

    /// Count all active organizations (admin only)
    async fn count_all_organizations(&self) -> Result<i64, anyhow::Error>;
}

/// Admin service trait for managing platform configuration
#[async_trait]
pub trait AdminService: Send + Sync {
    /// Batch upsert models pricing and metadata (admin only)
    async fn batch_upsert_models(
        &self,
        models: BatchUpdateModelAdminRequest,
    ) -> Result<BatchUpdateModelAdminResponse, AdminError>;

    /// Get complete history for a model with pagination (admin only) - includes pricing and other attributes
    async fn get_model_history(
        &self,
        model_name: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ModelHistoryEntry>, i64), AdminError>;

    /// Soft delete a model by setting is_active to false (admin only)
    async fn delete_model(
        &self,
        model_name: &str,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<(), AdminError>;

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
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<OrganizationLimitsHistoryEntry>, i64), AdminError>;

    /// List all users with pagination (admin only)
    async fn list_users(&self, limit: i64, offset: i64)
        -> Result<(Vec<UserInfo>, i64), AdminError>;

    /// List all users with their earliest organization and spend limit (admin only)
    /// If search_by_name is provided, filters users by organization name (case-insensitive partial match)
    async fn list_users_with_organizations(
        &self,
        limit: i64,
        offset: i64,
        search_by_name: Option<String>,
    ) -> Result<(Vec<(UserInfo, Option<UserOrganizationInfo>)>, i64), AdminError>;

    /// List all models with pagination (admin only)
    /// If include_inactive is true, includes disabled models
    async fn list_models(
        &self,
        include_inactive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminModelInfo>, i64), AdminError>;

    /// Update organization concurrent request limit (admin only)
    /// Set to None to use the default limit
    async fn update_organization_concurrent_limit(
        &self,
        organization_id: uuid::Uuid,
        concurrent_limit: Option<u32>,
    ) -> Result<(), AdminError>;

    /// Get organization concurrent request limit (admin only)
    /// Returns the custom limit if set, None if using default
    async fn get_organization_concurrent_limit(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<Option<u32>, AdminError>;

    /// List all organizations with pagination (admin only)
    async fn list_organizations(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminOrganizationInfo>, i64), AdminError>;
}
