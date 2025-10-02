use async_trait::async_trait;

/// Request to update model pricing and metadata
#[derive(Debug, Clone)]
pub struct UpdateModelAdminRequest {
    pub input_cost_amount: Option<i64>,
    pub input_cost_scale: Option<i32>,
    pub input_cost_currency: Option<String>,
    pub output_cost_amount: Option<i64>,
    pub output_cost_scale: Option<i32>,
    pub output_cost_currency: Option<String>,
    pub model_display_name: Option<String>,
    pub model_description: Option<String>,
    pub model_icon: Option<String>,
    pub context_length: Option<i32>,
    pub verifiable: Option<bool>,
    pub is_active: Option<bool>,
}

/// Model pricing information (result after update)
#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub model_display_name: String,
    pub model_description: String,
    pub model_icon: Option<String>,
    pub input_cost_amount: i64,
    pub input_cost_scale: i32,
    pub input_cost_currency: String,
    pub output_cost_amount: i64,
    pub output_cost_scale: i32,
    pub output_cost_currency: String,
    pub context_length: i32,
    pub verifiable: bool,
    pub is_active: bool,
}

/// Model pricing history entry
#[derive(Debug, Clone)]
pub struct ModelPricingHistoryEntry {
    pub id: uuid::Uuid,
    pub model_id: uuid::Uuid,
    pub input_cost_amount: i64,
    pub input_cost_scale: i32,
    pub input_cost_currency: String,
    pub output_cost_amount: i64,
    pub output_cost_scale: i32,
    pub output_cost_currency: String,
    pub context_length: i32,
    pub model_display_name: String,
    pub model_description: String,
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
    #[error("Invalid pricing data: {0}")]
    InvalidPricing(String),
    #[error("Unauthorized: {0}")]
    Unauthorized(String),
    #[error("Internal error: {0}")]
    InternalError(String),
}

/// Repository trait for admin operations on models
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
}

/// Admin service trait for managing platform configuration
#[async_trait]
pub trait AdminService: Send + Sync {
    /// Batch upsert models pricing and metadata (admin only)
    async fn batch_upsert_models(
        &self,
        models: Vec<(String, UpdateModelAdminRequest)>,
    ) -> Result<Vec<ModelPricing>, AdminError>;

    /// Get pricing history for a model (admin only)
    async fn get_pricing_history(
        &self,
        model_name: &str,
    ) -> Result<Vec<ModelPricingHistoryEntry>, AdminError>;
}
