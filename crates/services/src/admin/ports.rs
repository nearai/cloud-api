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
    pub text_pricing: Option<Option<crate::usage::TextPricingProfile>>,
    pub model_display_name: Option<String>,
    pub model_description: Option<String>,
    pub model_icon: Option<String>,
    pub context_length: Option<i32>,
    pub verifiable: Option<bool>,
    pub is_active: Option<bool>,
    /// If true, allows activation even with zero pricing.
    pub allow_free: Option<bool>,
    /// `Some(false)` unlists the model: it leaves the public model catalogs
    /// and only organizations that hold a grant may use it. `Some(true)`
    /// lists it again, `None` leaves it unchanged.
    pub is_listed: Option<bool>,
    pub aliases: Option<Vec<String>>,
    pub owned_by: Option<String>,
    // Provider configuration
    pub provider_type: Option<String>,
    pub provider_config: Option<serde_json::Value>,
    /// Compare-and-set guard for `provider_config`, set from the stored value
    /// a merge started from. `None` skips the check; `Some(expected)` applies
    /// the update only while the stored config still equals `expected`
    /// (`Some(None)` = NULL), and otherwise fails with a transaction conflict.
    pub expected_provider_config: Option<Option<serde_json::Value>>,
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
    /// Recommended replacement model for a planned deprecation (canonical
    /// model name). Announced in the `x-model-successor` response header.
    /// Cleared together with `deprecation_date`.
    ///
    /// Tri-state: `None` = leave unchanged, `Some(None)` = clear to NULL,
    /// `Some(Some(v))` = set to `v`.
    pub successor_model_name: Option<Option<String>>,
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
    /// Versioned exact text-pricing profile, when tiered billing is enabled.
    pub text_pricing: Option<crate::usage::TextPricingProfile>,
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
    /// Recommended replacement model for the planned deprecation.
    pub successor_model_name: Option<String>,
    /// OpenRouter `openrouter.slug` override. NULL = unset.
    pub openrouter_slug: Option<String>,
    /// Whether the model is listed. An unlisted model is left out of the
    /// public model catalogs and only organizations that hold a grant may
    /// use it.
    pub is_listed: bool,
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
    pub text_pricing: Option<crate::usage::TextPricingProfile>,
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
    /// Recommended replacement model for the planned deprecation.
    pub successor_model_name: Option<String>,
    /// OpenRouter `openrouter.slug` override the model carried at this point.
    pub openrouter_slug: Option<String>,
    /// If true, this model was allowed to serve without pricing at this point in time.
    pub allow_free: bool,
    /// Whether the model was listed (in the public model catalogs and open to
    /// every caller) at this point in time.
    pub is_listed: bool,
    pub effective_from: chrono::DateTime<chrono::Utc>,
    pub effective_until: Option<chrono::DateTime<chrono::Utc>>,
    pub changed_by_user_id: Option<uuid::Uuid>,
    pub changed_by_user_email: Option<String>,
    pub change_reason: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
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

/// Lifecycle status of a scheduled model pricing change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
    pub text_pricing: Option<crate::usage::TextPricingProfile>,
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
    pub old_text_pricing: Option<crate::usage::TextPricingProfile>,
    pub new_text_pricing: Option<crate::usage::TextPricingProfile>,
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
    pub old_text_pricing: Option<crate::usage::TextPricingProfile>,
    pub new_text_pricing: Option<crate::usage::TextPricingProfile>,
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
    pub lifecycle: crate::lifecycle::OrganizationLifecycle,
}

/// Which organizations the admin list returns. Defaults to `Active`, the historical behavior.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrganizationLifecycleFilter {
    #[default]
    Active,
    Deleted,
    Erased,
    All,
}

/// A grant: one organization that may use one unlisted model.
#[derive(Debug, Clone)]
pub struct ModelAccessGrant {
    pub organization_id: uuid::Uuid,
    pub organization_name: String,
    pub granted_at: chrono::DateTime<chrono::Utc>,
    /// The admin who made the grant. `None` if that user row is gone.
    pub granted_by_user_id: Option<uuid::Uuid>,
    pub granted_by_user_email: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("Analytics query exceeded its statement budget")]
    Timeout,
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

    /// List active owner/admin recipients for orgs that used the model since `since`.
    async fn list_model_deprecation_recipients(
        &self,
        model_name: &str,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<ModelDeprecationRecipient>, anyhow::Error>;

    /// True when the user exists and is active. Email batches call this right
    /// before each send so a recipient erased mid-batch is skipped.
    async fn is_user_active(&self, user_id: uuid::Uuid) -> Result<bool, anyhow::Error>;

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

    /// Persist one pricing change email delivery outcome.
    async fn record_pricing_change_delivery(
        &self,
        record: PricingChangeDeliveryRecord,
    ) -> Result<(), anyhow::Error>;

    /// List organizations matching the lifecycle filter with pagination (admin only)
    async fn list_all_organizations(
        &self,
        limit: i64,
        offset: i64,
        lifecycle: OrganizationLifecycleFilter,
    ) -> Result<Vec<AdminOrganizationInfo>, anyhow::Error>;

    /// Fetch a single active organization by id with spend limit and usage
    /// (admin only). Returns `None` if not found or inactive.
    async fn get_organization(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<Option<AdminOrganizationInfo>, anyhow::Error>;

    /// Grant the organization access to the model. Returns the stored grant
    /// and whether this call created it: a grant that already exists is
    /// returned unchanged. A grant that is created gets its audit row in the
    /// same statement. `None` if the organization is not active: the check
    /// holds the organization row until the grant is written, so a
    /// concurrent deletion cannot leave a grant behind.
    async fn grant_model_access(
        &self,
        model_id: uuid::Uuid,
        organization_id: uuid::Uuid,
        granted_by_user_id: Option<uuid::Uuid>,
    ) -> Result<Option<(ModelAccessGrant, bool)>, anyhow::Error>;

    /// Revoke the organization's grant for the model. Returns whether a
    /// grant existed. A grant that is revoked gets its audit row in the same
    /// statement.
    async fn revoke_model_access(
        &self,
        model_id: uuid::Uuid,
        organization_id: uuid::Uuid,
        revoked_by_user_id: Option<uuid::Uuid>,
    ) -> Result<bool, anyhow::Error>;
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
