use crate::service_usage::ports::ServiceUnit;
use async_trait::async_trait;

/// Request to update model pricing and metadata
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct UpdateModelAdminRequest {
    pub input_cost_per_token: Option<i64>,
    pub output_cost_per_token: Option<i64>,
    pub cost_per_image: Option<i64>,
    /// Cost per cached input token.
    ///
    /// Tri-state: `None` = leave unchanged, `Some(None)` = disable cache
    /// pricing (cached tokens billed at the full input rate), `Some(Some(v))`
    /// = set to `v` (`Some(Some(0))` = genuinely free cache reads).
    pub cache_read_cost_per_token: Option<Option<i64>>,
    pub model_display_name: Option<String>,
    pub model_description: Option<String>,
    pub model_icon: Option<String>,
    pub context_length: Option<i32>,
    pub verifiable: Option<bool>,
    pub is_active: Option<bool>,
    /// If true, allows activation even with zero pricing.
    pub allow_free: Option<bool>,
    pub aliases: Option<Vec<String>>,
    pub owned_by: Option<String>,
    // Provider configuration
    pub provider_type: Option<String>,
    pub provider_config: Option<serde_json::Value>,
    pub attestation_supported: Option<bool>,
    // Architecture/modalities
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
    /// Base URL for the model's inference endpoint
    pub inference_url: Option<String>,
    // OpenRouter-compatibility fields
    pub hugging_face_id: Option<String>,
    pub quantization: Option<String>,
    pub max_output_length: Option<i32>,
    pub supported_sampling_parameters: Option<Vec<String>>,
    pub supported_features: Option<Vec<String>>,
    /// Datacenter country codes (ISO 3166 Alpha-2) the model runs in.
    pub datacenters: Option<Vec<String>>,
    /// Whether the model is "ready" (OpenRouter `is_ready`).
    ///
    /// Tri-state: `None` = leave unchanged, `Some(None)` = clear to NULL,
    /// `Some(Some(v))` = set to `v`.
    pub is_ready: Option<Option<bool>>,
    /// Planned deprecation date (OpenRouter `deprecation_date`), parsed and
    /// normalized from an ISO 8601 string at the route layer.
    ///
    /// Tri-state: `None` = leave unchanged, `Some(None)` = clear to NULL,
    /// `Some(Some(dt))` = set to `dt`.
    pub deprecation_date: Option<Option<chrono::DateTime<chrono::Utc>>>,
    /// OpenRouter `openrouter.slug` override (validated at the route layer).
    ///
    /// Tri-state: `None` = leave unchanged, `Some(None)` = clear to NULL,
    /// `Some(Some(v))` = set to `v`.
    pub openrouter_slug: Option<Option<String>>,
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
    /// `None` = cache pricing disabled; `Some(x)` = cached tokens billed at
    /// `x` (`Some(0)` = genuinely free).
    pub cache_read_cost_per_token: Option<i64>,
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
    /// Base URL for the model's inference endpoint
    pub inference_url: Option<String>,
    // OpenRouter-compatibility fields
    pub hugging_face_id: Option<String>,
    pub quantization: Option<String>,
    pub max_output_length: Option<i32>,
    pub supported_sampling_parameters: Vec<String>,
    pub supported_features: Vec<String>,
    /// Datacenter country codes (ISO 3166 Alpha-2) the model runs in.
    pub datacenters: Option<Vec<String>>,
    pub is_ready: Option<bool>,
    pub deprecation_date: Option<chrono::DateTime<chrono::Utc>>,
    /// OpenRouter `openrouter.slug` override. NULL = unset.
    pub openrouter_slug: Option<String>,
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
    /// `None` = cache pricing disabled at this point in time.
    pub cache_read_cost_per_token: Option<i64>,
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
    pub inference_url: Option<String>,
    // OpenRouter-compatibility snapshot
    pub hugging_face_id: Option<String>,
    pub quantization: Option<String>,
    pub max_output_length: Option<i32>,
    pub supported_sampling_parameters: Vec<String>,
    pub supported_features: Vec<String>,
    /// Datacenter country codes (ISO 3166 Alpha-2) the model ran in.
    pub datacenters: Option<Vec<String>>,
    pub is_ready: Option<bool>,
    pub deprecation_date: Option<chrono::DateTime<chrono::Utc>>,
    /// OpenRouter `openrouter.slug` override the model carried at this point.
    pub openrouter_slug: Option<String>,
    /// If true, this model was allowed to serve without pricing at this point in time.
    pub allow_free: bool,
    pub effective_from: chrono::DateTime<chrono::Utc>,
    pub effective_until: Option<chrono::DateTime<chrono::Utc>>,
    pub changed_by_user_id: Option<uuid::Uuid>,
    pub changed_by_user_email: Option<String>,
    pub change_reason: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Request to update organization limits
/// All amounts use fixed scale of 9 (nano-dollars)
#[derive(Debug, Clone)]
pub struct OrganizationLimitsUpdate {
    pub spend_limit: i64,
    pub credit_type: String,
    pub source: Option<String>,
    pub currency: String,
    pub changed_by: Option<String>,
    pub change_reason: Option<String>,
    pub changed_by_user_id: Option<uuid::Uuid>, // The authenticated user ID who made the change
    pub changed_by_user_email: Option<String>, // The email of the authenticated user who made the change
}

/// Organization limits (current active limits for a specific credit type)
/// All amounts use fixed scale of 9 (nano-dollars)
#[derive(Debug, Clone)]
pub struct OrganizationLimits {
    pub organization_id: uuid::Uuid,
    pub spend_limit: i64,
    pub credit_type: String,
    pub source: Option<String>,
    pub currency: String,
    pub effective_from: chrono::DateTime<chrono::Utc>,
}

/// Organization limits history entry
/// All amounts use fixed scale of 9 (nano-dollars)
#[derive(Debug, Clone)]
pub struct OrganizationLimitsHistoryEntry {
    pub id: uuid::Uuid,
    pub organization_id: uuid::Uuid,
    pub spend_limit: i64,
    pub credit_type: String,
    pub source: Option<String>,
    pub currency: String,
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
    pub auth_provider: String,
    pub provider_user_id: String,
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
    /// `None` = cache pricing disabled; `Some(x)` = cached tokens billed at
    /// `x` (`Some(0)` = genuinely free).
    pub cache_read_cost_per_token: Option<i64>,
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
    /// Base URL for the model's inference endpoint
    pub inference_url: Option<String>,
    // OpenRouter-compatibility fields
    pub hugging_face_id: Option<String>,
    pub quantization: Option<String>,
    pub max_output_length: Option<i32>,
    pub supported_sampling_parameters: Vec<String>,
    pub supported_features: Vec<String>,
    /// Datacenter country codes (ISO 3166 Alpha-2) the model runs in.
    pub datacenters: Option<Vec<String>>,
    pub is_ready: Option<bool>,
    pub deprecation_date: Option<chrono::DateTime<chrono::Utc>>,
    /// OpenRouter `openrouter.slug` override. NULL = unset.
    pub openrouter_slug: Option<String>,
}

/// Active model summary used by the planned-deprecation notification workflow.
#[derive(Debug, Clone)]
pub struct ModelDeprecationModel {
    pub id: uuid::Uuid,
    pub model_name: String,
    pub model_display_name: String,
}

/// One affected admin membership row. The service deduplicates sends by
/// recipient email, but records delivery status for every affected org row.
#[derive(Debug, Clone)]
pub struct ModelDeprecationRecipient {
    pub user_id: uuid::Uuid,
    pub email: String,
    pub organization_id: uuid::Uuid,
    pub organization_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelDeprecationEmailStatus {
    Sent,
    Failed,
    Skipped,
}

impl ModelDeprecationEmailStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelDeprecationDeliveryRecord {
    pub model_id: uuid::Uuid,
    pub model_name: String,
    pub model_display_name: String,
    pub successor_model_name: String,
    pub deprecation_date: chrono::DateTime<chrono::Utc>,
    pub recipient_user_id: uuid::Uuid,
    pub recipient_email: String,
    pub organization_id: uuid::Uuid,
    pub organization_name: String,
    pub status: ModelDeprecationEmailStatus,
    pub email_message_id: Option<String>,
    pub email_last_error: Option<String>,
    pub initiated_by_user_id: Option<uuid::Uuid>,
    pub initiated_by_user_email: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelDeprecationPreview {
    pub recipient_count: i64,
    pub organization_count: i64,
    pub usage_window_days: i64,
}

#[derive(Debug, Clone)]
pub struct ModelDeprecationConfirmResult {
    pub model_id: String,
    pub successor_model_id: String,
    pub deprecation_date: chrono::DateTime<chrono::Utc>,
    pub recipient_count: i64,
    pub organization_count: i64,
    pub sent_count: i64,
    pub failed_count: i64,
    pub skipped_count: i64,
}

/// Lifecycle status of a scheduled model pricing change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduledPricingChangeStatus {
    Pending,
    Applying,
    Applied,
    Cancelled,
    Failed,
}

impl ScheduledPricingChangeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applying => "applying",
            Self::Applied => "applied",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "applying" => Some(Self::Applying),
            "applied" => Some(Self::Applied),
            "cancelled" => Some(Self::Cancelled),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// Validated per-model input for scheduling a pricing change.
/// New amounts are nano-dollars (scale 9); `None` = field unchanged.
#[derive(Debug, Clone)]
pub struct PricingChangeInput {
    pub model_name: String,
    pub effective_at: chrono::DateTime<chrono::Utc>,
    pub new_input_cost_per_token: Option<i64>,
    pub new_output_cost_per_token: Option<i64>,
    pub new_cache_read_cost_per_token: Option<i64>,
    pub new_cost_per_image: Option<i64>,
}

/// Current pricing + identity snapshot of an active model, captured at
/// confirm time as the "old" side of a scheduled pricing change.
#[derive(Debug, Clone)]
pub struct ModelPricingSnapshot {
    pub id: uuid::Uuid,
    pub model_name: String,
    pub model_display_name: String,
    pub input_cost_per_token: i64,
    pub output_cost_per_token: i64,
    /// `None` = cache pricing disabled at confirm time.
    pub cache_read_cost_per_token: Option<i64>,
    pub cost_per_image: i64,
}

/// One row to insert into `scheduled_model_pricing_changes`.
#[derive(Debug, Clone)]
pub struct ScheduledPricingChangeInsert {
    pub model_id: uuid::Uuid,
    pub model_name: String,
    pub model_display_name: String,
    pub new_input_cost_per_token: Option<i64>,
    pub new_output_cost_per_token: Option<i64>,
    pub new_cache_read_cost_per_token: Option<i64>,
    pub new_cost_per_image: Option<i64>,
    pub old_input_cost_per_token: i64,
    pub old_output_cost_per_token: i64,
    /// `None` = cache pricing was disabled at confirm time.
    pub old_cache_read_cost_per_token: Option<i64>,
    pub old_cost_per_image: i64,
    pub effective_at: chrono::DateTime<chrono::Utc>,
}

/// Full `scheduled_model_pricing_changes` row.
#[derive(Debug, Clone)]
pub struct ScheduledPricingChange {
    pub id: uuid::Uuid,
    pub batch_id: uuid::Uuid,
    pub model_id: uuid::Uuid,
    pub model_name: String,
    pub model_display_name: String,
    pub new_input_cost_per_token: Option<i64>,
    pub new_output_cost_per_token: Option<i64>,
    pub new_cache_read_cost_per_token: Option<i64>,
    pub new_cost_per_image: Option<i64>,
    pub old_input_cost_per_token: i64,
    pub old_output_cost_per_token: i64,
    /// `None` = cache pricing was disabled at confirm time.
    pub old_cache_read_cost_per_token: Option<i64>,
    pub old_cost_per_image: i64,
    pub effective_at: chrono::DateTime<chrono::Utc>,
    pub status: ScheduledPricingChangeStatus,
    pub apply_attempts: i32,
    pub applied_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
    pub created_by_user_id: Option<uuid::Uuid>,
    pub created_by_user_email: Option<String>,
    pub change_reason: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// One affected (user, org, model) row. The service consolidates sends into
/// one email per distinct recipient address covering all their models, but
/// records delivery status for every affected org membership.
#[derive(Debug, Clone)]
pub struct PricingChangeRecipientRow {
    pub user_id: uuid::Uuid,
    pub email: String,
    pub organization_id: uuid::Uuid,
    pub organization_name: String,
    /// Canonical model name (alias usage is resolved to the canonical name).
    pub model_name: String,
}

/// Per-model breakdown inside a pricing change preview.
#[derive(Debug, Clone)]
pub struct PricingChangeModelPreview {
    pub model_name: String,
    pub model_display_name: String,
    pub effective_at: chrono::DateTime<chrono::Utc>,
    pub recipient_count: i64,
    pub organization_count: i64,
    pub old_input_cost_per_token: i64,
    pub old_output_cost_per_token: i64,
    /// `None` = cache pricing currently disabled.
    pub old_cache_read_cost_per_token: Option<i64>,
    pub old_cost_per_image: i64,
    pub new_input_cost_per_token: Option<i64>,
    pub new_output_cost_per_token: Option<i64>,
    pub new_cache_read_cost_per_token: Option<i64>,
    pub new_cost_per_image: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct PricingChangePreview {
    /// Distinct recipient emails across the whole batch.
    pub recipient_count: i64,
    /// Distinct organizations across the whole batch.
    pub organization_count: i64,
    pub usage_window_days: i64,
    pub models: Vec<PricingChangeModelPreview>,
}

#[derive(Debug, Clone)]
pub struct PricingChangeConfirmResult {
    pub batch_id: uuid::Uuid,
    pub recipient_count: i64,
    pub organization_count: i64,
    pub sent_count: i64,
    pub failed_count: i64,
    pub skipped_count: i64,
    pub changes: Vec<ScheduledPricingChange>,
}

#[derive(Debug, Clone)]
pub struct PricingChangeDeliveryRecord {
    pub batch_id: uuid::Uuid,
    pub recipient_user_id: uuid::Uuid,
    pub recipient_email: String,
    pub organization_id: uuid::Uuid,
    pub organization_name: String,
    /// Canonical model names included in the email this recipient received.
    pub model_names: Vec<String>,
    pub status: ModelDeprecationEmailStatus,
    pub email_message_id: Option<String>,
    pub email_last_error: Option<String>,
    pub initiated_by_user_id: Option<uuid::Uuid>,
    pub initiated_by_user_email: Option<String>,
}

/// Marker error returned (wrapped in `anyhow::Error`) by
/// [`AdminRepository::insert_scheduled_pricing_changes`] when another open
/// (pending/applying) change already exists for a model. The service
/// downcasts it to surface a 409 instead of a 500.
#[derive(Debug, thiserror::Error)]
#[error("an open scheduled pricing change already exists for model '{model_name}'")]
pub struct PricingChangeOpenConflictError {
    pub model_name: String,
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

/// A single organization member with full user details (admin view).
///
/// Unlike the member-facing `/v1/organizations/{id}/members` endpoint (which
/// hides email/last-login from non-privileged members), the admin view exposes
/// the full user record — including inactive (soft-deleted) members — consistent
/// with `/v1/admin/users`.
#[derive(Debug, Clone)]
pub struct AdminOrganizationMemberInfo {
    pub member_id: uuid::Uuid,
    pub organization_id: uuid::Uuid,
    /// Raw role string from the database: "owner", "admin", or "member".
    pub role: String,
    pub joined_at: chrono::DateTime<chrono::Utc>,
    pub invited_by: Option<uuid::Uuid>,
    pub user: UserInfo,
}

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("Model not found: {0}")]
    ModelNotFound(String),
    #[error("Service not found: {0}")]
    ServiceNotFound(String),
    #[error("Organization not found: {0}")]
    OrganizationNotFound(String),
    #[error("Invalid pricing data: {0}")]
    InvalidPricing(String),
    #[error("Invalid limits data: {0}")]
    InvalidLimits(String),
    #[error("Invalid deprecation request: {0}")]
    InvalidDeprecation(String),
    #[error("Pricing change conflict: {0}")]
    PricingChangeConflict(String),
    #[error("Pricing change not found: {0}")]
    PricingChangeNotFound(String),
    #[error("Unauthorized: {0}")]
    Unauthorized(String),
    #[error("Internal error: {0}")]
    InternalError(String),
}

/// Outcome of a `deprecate_model` operation.
#[derive(Debug, Clone)]
pub struct DeprecateModelOutcome {
    /// State of the deprecated model after the operation (`is_active = false`).
    pub deprecated: ModelPricing,
    /// State of the successor model after the operation, including the
    /// merged alias list.
    pub successor: ModelPricing,
    /// Number of pre-existing aliases of the deprecated model that were
    /// re-pointed at the successor (does not include the deprecated model's
    /// own canonical name).
    pub aliases_carried: u32,
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

    /// Fetch the current pricing costs and allow_free flag for a model by name.
    /// Returns `None` if the model does not exist (new model).
    /// Returns `Some((input_cost, output_cost, cost_per_image, cache_read_cost_per_token, allow_free))`,
    /// where `cache_read_cost_per_token` is `None` when cache pricing is disabled.
    async fn get_model_costs(
        &self,
        model_name: &str,
    ) -> Result<Option<(i64, i64, i64, Option<i64>, bool)>, anyhow::Error>;

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

    /// Atomically deprecate `deprecated_model_name` in favor of `successor_model_name`.
    ///
    /// In a single transaction:
    /// - Adds `deprecated_model_name` to `successor_model_name`'s alias list
    ///   (idempotent via `ON CONFLICT (alias_name) DO UPDATE`).
    /// - Repoints any existing **active** inbound aliases of the deprecated
    ///   model at the successor, so historical aliases keep resolving.
    ///   Inactive inbound aliases are left untouched.
    /// - Sets the deprecated model's `is_active = false` and records a
    ///   `model_history` entry with the supplied change reason.
    /// - Reads back both models' merged-alias state inside the same
    ///   transaction so a successful commit is atomic with the response.
    ///
    /// Returns `Ok(None)` for any of the "treat-as-not-found" conditions:
    /// either model is missing, or the successor is not currently active.
    /// (The successor-activeness check lives in the repository to keep the
    /// reads inside the transaction; the service surfaces both as
    /// `ModelNotFound`.) Self-target / empty-id validation is the
    /// service layer's responsibility.
    async fn deprecate_model(
        &self,
        deprecated_model_name: &str,
        successor_model_name: &str,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<Option<DeprecateModelOutcome>, anyhow::Error>;

    /// Update organization limits (creates new history entry, closes previous)
    async fn update_organization_limits(
        &self,
        organization_id: uuid::Uuid,
        limits: OrganizationLimitsUpdate,
    ) -> Result<OrganizationLimits, anyhow::Error>;

    /// Get all current active limits for an organization (one per credit type)
    async fn get_current_organization_limits(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<Vec<OrganizationLimits>, anyhow::Error>;

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
    async fn list_users(
        &self,
        limit: i64,
        offset: i64,
        search: Option<String>,
        is_active: Option<bool>,
    ) -> Result<(Vec<UserInfo>, i64), anyhow::Error>;

    /// List all users with their earliest organization and spend limit (admin only)
    /// If search_by_name is provided, filters users by organization name (case-insensitive partial match)
    /// Returns a tuple of (users, total_count) where total_count is the count of filtered users
    async fn list_users_with_organizations(
        &self,
        limit: i64,
        offset: i64,
        search: Option<String>,
        is_active: Option<bool>,
        search_by_name: Option<String>,
    ) -> Result<(Vec<(UserInfo, Option<UserOrganizationInfo>)>, i64), anyhow::Error>;

    /// List all models with pagination (admin only)
    /// If include_inactive is true, includes disabled models
    async fn list_models(
        &self,
        include_inactive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminModelInfo>, i64), anyhow::Error>;

    /// Fetch an active model by canonical model name for planned-deprecation workflows.
    async fn get_active_model_for_deprecation(
        &self,
        model_name: &str,
    ) -> Result<Option<ModelDeprecationModel>, anyhow::Error>;

    /// List active owner/admin recipients for orgs that used the model since `since`.
    async fn list_model_deprecation_recipients(
        &self,
        model_name: &str,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<ModelDeprecationRecipient>, anyhow::Error>;

    /// Return delivery keys already successfully sent for idempotent confirms.
    async fn list_sent_model_deprecation_delivery_keys(
        &self,
        model_id: uuid::Uuid,
        successor_model_name: &str,
        deprecation_date: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<(uuid::Uuid, uuid::Uuid)>, anyhow::Error>;

    /// Persist one deprecation email delivery outcome.
    async fn record_model_deprecation_delivery(
        &self,
        record: ModelDeprecationDeliveryRecord,
    ) -> Result<(), anyhow::Error>;

    /// Fetch an active model's pricing snapshot by canonical model name.
    async fn get_model_pricing_snapshot(
        &self,
        model_name: &str,
    ) -> Result<Option<ModelPricingSnapshot>, anyhow::Error>;

    /// List active owner/admin recipients for orgs that used any of the
    /// models (including via aliases) since `since`. Returns one row per
    /// (user, org, canonical model name).
    async fn list_pricing_change_recipients(
        &self,
        model_names: &[String],
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<PricingChangeRecipientRow>, anyhow::Error>;

    /// Insert all rows of a batch in one transaction.
    ///
    /// Idempotent per batch: if rows for `batch_id` already exist, returns
    /// them without inserting. If another open (pending/applying) change
    /// exists for any model, fails the transaction with
    /// [`AdminError::PricingChangeConflict`] semantics (unique violation on
    /// the partial index); callers map that to a 409.
    async fn insert_scheduled_pricing_changes(
        &self,
        batch_id: uuid::Uuid,
        changes: Vec<ScheduledPricingChangeInsert>,
        created_by_user_id: Option<uuid::Uuid>,
        created_by_user_email: Option<String>,
        change_reason: Option<String>,
    ) -> Result<Vec<ScheduledPricingChange>, anyhow::Error>;

    /// List all rows of one batch (any status), ordered by model name.
    async fn list_scheduled_pricing_changes_by_batch(
        &self,
        batch_id: uuid::Uuid,
    ) -> Result<Vec<ScheduledPricingChange>, anyhow::Error>;

    /// List scheduled pricing changes, optionally filtered by status,
    /// newest effective date first. Returns (rows, total).
    async fn list_scheduled_pricing_changes(
        &self,
        status: Option<ScheduledPricingChangeStatus>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ScheduledPricingChange>, i64), anyhow::Error>;

    /// Cancel a pending change. Returns `None` if the row is missing or no
    /// longer pending (already applying/applied/cancelled) — race-safe
    /// against the scheduler claim.
    async fn cancel_scheduled_pricing_change(
        &self,
        id: uuid::Uuid,
        cancelled_by_user_id: Option<uuid::Uuid>,
        cancelled_by_user_email: Option<String>,
    ) -> Result<Option<ScheduledPricingChange>, anyhow::Error>;

    /// Atomically claim due pending changes (pending -> applying,
    /// apply_attempts + 1) using `FOR UPDATE SKIP LOCKED` so concurrent
    /// instances partition the due set.
    async fn claim_due_pricing_changes(
        &self,
        limit: i64,
    ) -> Result<Vec<ScheduledPricingChange>, anyhow::Error>;

    /// Mark a claimed change as applied.
    async fn mark_pricing_change_applied(&self, id: uuid::Uuid) -> Result<(), anyhow::Error>;

    /// Mark a claimed change as failed. `retryable = true` returns it to
    /// `pending` for a later attempt; `false` parks it in `failed`.
    async fn mark_pricing_change_failed(
        &self,
        id: uuid::Uuid,
        error: &str,
        retryable: bool,
    ) -> Result<(), anyhow::Error>;

    /// Recover rows stuck in `applying` (e.g. instance crashed mid-apply)
    /// older than `stale_after`: back to `pending` while under
    /// `max_attempts`, otherwise `failed`. Returns the number of rows moved.
    async fn recover_stale_applying_pricing_changes(
        &self,
        stale_after: chrono::Duration,
        max_attempts: i32,
    ) -> Result<u64, anyhow::Error>;

    /// Return (user_id, organization_id) delivery keys already successfully
    /// sent for this batch, for idempotent confirms.
    async fn list_sent_pricing_change_delivery_keys(
        &self,
        batch_id: uuid::Uuid,
    ) -> Result<Vec<(uuid::Uuid, uuid::Uuid)>, anyhow::Error>;

    /// Persist one pricing change email delivery outcome.
    async fn record_pricing_change_delivery(
        &self,
        record: PricingChangeDeliveryRecord,
    ) -> Result<(), anyhow::Error>;

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

    /// Fetch a single active organization by id with spend limit and usage
    /// (admin only). Returns `None` if not found or inactive.
    async fn get_organization(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<Option<AdminOrganizationInfo>, anyhow::Error>;

    /// List members of a specific organization with full user details, including
    /// inactive (soft-deleted) *users* (admin only). Inactive (soft-deleted)
    /// *organizations* are excluded (their member rows are not returned),
    /// matching `/v1/admin/organizations`. Does NOT enforce membership of the
    /// caller — authorization is handled by the admin middleware at the route
    /// layer.
    async fn list_organization_members(
        &self,
        organization_id: uuid::Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AdminOrganizationMemberInfo>, anyhow::Error>;

    /// Count members of a specific organization, including inactive members
    /// (admin only). Must apply the same filters as `list_organization_members`
    /// so paginated totals stay consistent with the rows.
    async fn count_organization_members(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<i64, anyhow::Error>;

    /// Whether an active organization with this id exists (admin only).
    async fn organization_exists(&self, organization_id: uuid::Uuid)
        -> Result<bool, anyhow::Error>;

    /// List platform services with pagination (admin only)
    async fn list_services(
        &self,
        include_inactive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<PlatformServiceInfo>, i64), anyhow::Error>;

    /// Get platform service by id (admin only)
    async fn get_service_by_id(
        &self,
        id: uuid::Uuid,
    ) -> Result<Option<PlatformServiceInfo>, anyhow::Error>;

    /// Create a platform service (admin only)
    async fn create_service(
        &self,
        service_name: &str,
        display_name: &str,
        description: Option<&str>,
        unit: ServiceUnit,
        cost_per_unit: i64,
    ) -> Result<PlatformServiceInfo, anyhow::Error>;

    /// Update platform service (display_name, description, cost_per_unit, is_active)
    async fn update_service(
        &self,
        id: uuid::Uuid,
        display_name: Option<&str>,
        description: Option<&str>,
        cost_per_unit: Option<i64>,
        is_active: Option<bool>,
    ) -> Result<Option<PlatformServiceInfo>, anyhow::Error>;
}

/// Platform service info (for admin CRUD)
#[derive(Debug, Clone)]
pub struct PlatformServiceInfo {
    pub id: uuid::Uuid,
    pub service_name: String,
    pub display_name: String,
    pub description: Option<String>,
    pub unit: ServiceUnit,
    /// Price per unit in nano-USD (scale 9).
    pub cost_per_unit: i64,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
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

    /// Deprecate one model in favor of another (admin only). Atomic: in a
    /// single DB transaction, the deprecated model's name is added as an
    /// alias of the successor, any inbound aliases of the deprecated model
    /// are re-pointed at the successor, and the deprecated model is marked
    /// `is_active = false`.
    async fn deprecate_model(
        &self,
        deprecated_model_name: &str,
        successor_model_name: &str,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<DeprecateModelOutcome, AdminError>;

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
    async fn list_users(
        &self,
        limit: i64,
        offset: i64,
        search: Option<String>,
        is_active: Option<bool>,
    ) -> Result<(Vec<UserInfo>, i64), AdminError>;

    /// List all users with their earliest organization and spend limit (admin only)
    /// If search_by_name is provided, filters users by organization name (case-insensitive partial match)
    async fn list_users_with_organizations(
        &self,
        limit: i64,
        offset: i64,
        search: Option<String>,
        is_active: Option<bool>,
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

    /// Preview recipients for a planned deprecation without mutating state.
    async fn preview_model_deprecation(
        &self,
        model_name: &str,
        successor_model_name: &str,
        deprecation_date: chrono::DateTime<chrono::Utc>,
    ) -> Result<ModelDeprecationPreview, AdminError>;

    /// Set `deprecationDate`, send deprecation emails, and persist delivery audit rows.
    async fn confirm_model_deprecation(
        &self,
        model_name: &str,
        successor_model_name: &str,
        deprecation_date: chrono::DateTime<chrono::Utc>,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<ModelDeprecationConfirmResult, AdminError>;

    /// Preview recipients and per-model impact of a batch of scheduled
    /// pricing changes without mutating state.
    async fn preview_pricing_changes(
        &self,
        changes: Vec<PricingChangeInput>,
    ) -> Result<PricingChangePreview, AdminError>;

    /// Persist a batch of scheduled pricing changes and send one
    /// consolidated notification email per distinct recipient, covering all
    /// batch models the recipient's org(s) used. Idempotent per `batch_id`.
    async fn confirm_pricing_changes(
        &self,
        batch_id: uuid::Uuid,
        changes: Vec<PricingChangeInput>,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<PricingChangeConfirmResult, AdminError>;

    /// List scheduled pricing changes, optionally filtered by status.
    async fn list_pricing_changes(
        &self,
        status: Option<ScheduledPricingChangeStatus>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ScheduledPricingChange>, i64), AdminError>;

    /// Cancel a pending scheduled pricing change.
    async fn cancel_pricing_change(
        &self,
        id: uuid::Uuid,
        cancelled_by_user_id: Option<uuid::Uuid>,
        cancelled_by_user_email: Option<String>,
    ) -> Result<ScheduledPricingChange, AdminError>;

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

    /// Get a single organization by id (admin only). Returns
    /// `OrganizationNotFound` if it does not exist or is inactive.
    async fn get_organization(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<AdminOrganizationInfo, AdminError>;

    /// List members of a specific organization with full user details (admin only).
    /// Returns `OrganizationNotFound` if the organization does not exist.
    async fn list_organization_members(
        &self,
        organization_id: uuid::Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminOrganizationMemberInfo>, i64), AdminError>;

    /// List platform services with pagination (admin only)
    async fn list_services(
        &self,
        include_inactive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<PlatformServiceInfo>, i64), AdminError>;

    /// Get platform service by id (admin only). Returns ServiceNotFound if missing.
    async fn get_service_by_id(&self, id: uuid::Uuid) -> Result<PlatformServiceInfo, AdminError>;

    /// Create a platform service (admin only)
    async fn create_service(
        &self,
        service_name: &str,
        display_name: &str,
        description: Option<&str>,
        unit: ServiceUnit,
        cost_per_unit: i64,
    ) -> Result<PlatformServiceInfo, AdminError>;

    /// Update platform service (display_name, description, cost_per_unit, is_active). Returns ServiceNotFound if missing.
    async fn update_service(
        &self,
        id: uuid::Uuid,
        display_name: Option<&str>,
        description: Option<&str>,
        cost_per_unit: Option<i64>,
        is_active: Option<bool>,
    ) -> Result<PlatformServiceInfo, AdminError>;
}
