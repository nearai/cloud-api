use async_trait::async_trait;
use uuid::Uuid;

/// Model object (matches OpenAI API)
/// Describes an OpenAI model offering that can be used with the API.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// The Unix timestamp (in seconds) when the model was created
    pub created: u64,
    /// The model identifier, which can be referenced in the API endpoints
    pub id: String,
    /// The object type, which is always "model"
    pub object: String,
    /// The organization that owns the model
    pub owned_by: String,
}

/// Model with pricing and metadata information
#[derive(Debug, Clone)]
pub struct ModelWithPricing {
    pub id: Uuid,
    pub model_name: String,
    pub model_display_name: String,
    pub model_description: String,
    pub model_icon: Option<String>,

    // Input pricing using decimal representation
    pub input_cost_amount: i64,
    pub input_cost_scale: i32,
    pub input_cost_currency: String,

    // Output pricing using decimal representation
    pub output_cost_amount: i64,
    pub output_cost_scale: i32,
    pub output_cost_currency: String,

    // Model metadata
    pub context_length: i32,
    pub verifiable: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelsError {
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
}

/// Repository trait for accessing model data
#[async_trait]
pub trait ModelsRepository: Send + Sync {
    /// Get all active models with pricing
    async fn get_all_active_models(&self) -> Result<Vec<ModelWithPricing>, anyhow::Error>;
}

#[async_trait]
pub trait ModelsService: Send + Sync {
    /// Get basic model info (from inference providers)
    async fn get_models(&self) -> Result<Vec<ModelInfo>, ModelsError>;

    /// Get models with pricing and metadata (from database)
    async fn get_models_with_pricing(&self) -> Result<Vec<ModelWithPricing>, ModelsError>;
}
