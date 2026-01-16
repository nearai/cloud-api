use async_trait::async_trait;
use uuid::Uuid;

/// Model object (matches OpenAI API)
/// Describes an OpenAI model offering that can be used with the API.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// The Unix timestamp (in seconds) when the model was created
    pub created: i64,
    /// The model identifier, which can be referenced in the API endpoints
    pub id: String,
    /// The object type, which is always "model"
    pub object: String,
    /// The organization that owns the model
    pub owned_by: String,
}

/// Model with pricing and metadata information
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct ModelWithPricing {
    pub id: Uuid,
    /// Canonical model name (e.g., "nearai/gpt-oss-120b") used for vLLM
    pub model_name: String,
    pub model_display_name: String,
    pub model_description: String,
    pub model_icon: Option<String>,

    // Pricing (fixed scale 9 = nano-dollars, USD only)
    pub input_cost_per_token: i64,
    pub output_cost_per_token: i64,
    pub cost_per_image: i64,

    // Model metadata
    pub context_length: i32,
    pub verifiable: bool,
    pub aliases: Vec<String>,
    pub owned_by: String,

    // Provider configuration
    /// Provider type: "vllm" (TEE-enabled) or "external" (3rd party)
    pub provider_type: String,
    /// JSON config for external providers (backend, base_url, etc.)
    pub provider_config: Option<serde_json::Value>,
    /// Whether this model supports TEE attestation
    pub attestation_supported: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelsError {
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
    #[error("Model not found: {0}")]
    NotFound(String),
}

/// Repository trait for accessing model data
#[async_trait]
pub trait ModelsRepository: Send + Sync {
    /// Get all active models count
    async fn get_all_active_models_count(&self) -> Result<i64, anyhow::Error>;

    /// Get all active models with pricing
    async fn get_all_active_models(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ModelWithPricing>, anyhow::Error>;

    /// Get a specific model by name
    async fn get_model_by_name(
        &self,
        model_name: &str,
    ) -> Result<Option<ModelWithPricing>, anyhow::Error>;

    /// Resolve a model identifier (alias or canonical name) and return the full model details
    /// Returns None if the model is not found or not active
    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> Result<Option<ModelWithPricing>, anyhow::Error>;

    /// Get list of configured model names (canonical names) from database
    /// Returns only active models that have been configured with pricing
    async fn get_configured_model_names(&self) -> Result<Vec<String>, anyhow::Error>;
}

#[async_trait]
pub trait ModelsServiceTrait: Send + Sync {
    /// Get basic model info (from inference providers)
    async fn get_models(&self) -> Result<Vec<ModelInfo>, ModelsError>;

    /// Get models with pricing and metadata (from database)
    async fn get_models_with_pricing(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ModelWithPricing>, i64), ModelsError>;

    /// Get a specific model by name
    async fn get_model_by_name(&self, model_name: &str) -> Result<ModelWithPricing, ModelsError>;

    /// Resolve a model identifier (alias or canonical name) and return the full model details
    /// Returns an error if the model is not found or not active
    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> Result<ModelWithPricing, ModelsError>;

    /// Get list of configured model names (canonical names) from database
    /// Returns only active models that have been configured with pricing
    async fn get_configured_model_names(&self) -> Result<Vec<String>, ModelsError>;
}
