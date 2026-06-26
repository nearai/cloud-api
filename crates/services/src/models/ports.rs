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
    pub cache_read_cost_per_token: i64,

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

    // Architecture/modalities
    /// Input modalities the model accepts, e.g., ["text"], ["text", "image"]
    pub input_modalities: Option<Vec<String>>,
    /// Output modalities the model produces, e.g., ["text"], ["image"]
    pub output_modalities: Option<Vec<String>>,

    /// Base URL for the model's inference endpoint
    pub inference_url: Option<String>,

    // OpenRouter-compatibility fields
    // (https://openrouter.ai/docs/guides/community/for-providers)
    /// HuggingFace identifier (e.g. "Qwen/Qwen3-VL-30B-A3B-Instruct"). NULL when not on HF.
    pub hugging_face_id: Option<String>,
    /// Quantization label: int4/int8/fp4/fp6/fp8/fp16/bf16/fp32.
    pub quantization: Option<String>,
    /// Maximum number of output tokens the model can produce in a single response.
    pub max_output_length: Option<i32>,
    /// Sampling parameters accepted by the model (OpenRouter vocabulary).
    pub supported_sampling_parameters: Vec<String>,
    /// Feature capabilities (OpenRouter vocabulary: tools, json_mode, ...).
    pub supported_features: Vec<String>,
    /// Datacenter country codes (ISO 3166 Alpha-2) the model runs in.
    /// NULL when unset; serialized on the public API as
    /// `datacenters: [{ "country_code": "US" }, ...]`.
    pub datacenters: Option<Vec<String>>,
    /// Whether the model is "ready" (OpenRouter `is_ready`). Stored/exposed
    /// verbatim; does not affect Cloud API's own listing. NULL = unset.
    pub is_ready: Option<bool>,
    /// Planned deprecation date (OpenRouter `deprecation_date`, ISO 8601).
    /// NULL = no planned deprecation.
    pub deprecation_date: Option<chrono::DateTime<chrono::Utc>>,
    /// OpenRouter `openrouter.slug` override. When set, the public API emits a
    /// nested `openrouter: { slug: <value> }` object; NULL = unset (omitted).
    pub openrouter_slug: Option<String>,
    /// When the model row was created — used as OpenRouter's `created` unix timestamp.
    pub created_at: chrono::DateTime<chrono::Utc>,
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
    /// Get all active models with pricing
    async fn get_all_active_models(&self) -> Result<Vec<ModelWithPricing>, anyhow::Error>;

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

    /// Get models with pricing and metadata (from database).
    ///
    /// Implementations are expected to cache the result with a short TTL since
    /// this endpoint is publicly accessible and called frequently.
    async fn get_models_with_pricing(&self) -> Result<Vec<ModelWithPricing>, ModelsError>;

    /// Get a specific model by name
    async fn get_model_by_name(&self, model_name: &str) -> Result<ModelWithPricing, ModelsError>;

    /// Resolve a model identifier (alias or canonical name) and return the full model details
    /// Returns an error if the model is not found or not active
    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> Result<ModelWithPricing, ModelsError>;

    /// Resolve a model identifier for public model-detail reads.
    ///
    /// Production implementations should use the cached active public model
    /// list, preferring exact canonical model-name matches before alias matches.
    /// The default keeps existing test doubles source-compatible; concrete
    /// public catalog services should override it.
    async fn resolve_public_model(
        &self,
        identifier: &str,
    ) -> Result<ModelWithPricing, ModelsError> {
        self.resolve_and_get_model(identifier).await
    }

    /// Resolve `identifier` against the cached active-models alias map.
    /// Returns `Some(canonical_name)` when `identifier` is a registered
    /// alias of an active model, `None` when it is a canonical name,
    /// unknown, or the lookup fails.
    ///
    /// Unlike `resolve_and_get_model`, this never hits the DB on the hot
    /// path (cache-backed, short TTL) and is intended for advisory
    /// annotations such as alias-substitution warnings — authoritative
    /// checks must use `resolve_and_get_model`.
    async fn resolve_alias_cached(&self, identifier: &str) -> Option<String>;

    /// Get list of configured model names (canonical names) from database
    /// Returns only active models that have been configured with pricing
    async fn get_configured_model_names(&self) -> Result<Vec<String>, ModelsError>;

    /// Invalidate any cached model list / metadata. Called by admin writes
    /// that mutate the `models` or `model_aliases` tables.
    async fn invalidate_models_cache(&self);
}
