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
    /// Cost per cached input token. `None` = cache pricing disabled (cached
    /// tokens billed at `input_cost_per_token`, `input_cache_read` omitted
    /// from the public catalog); `Some(x)` = cached tokens billed at `x`
    /// (`Some(0)` = genuinely free, rendered as `"0"`).
    pub cache_read_cost_per_token: Option<i64>,
    pub text_pricing: Option<crate::usage::TextPricingProfile>,

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
    /// Recommended replacement model for the planned deprecation
    /// (`x-model-successor` response header).
    pub successor_model_name: Option<String>,
    /// OpenRouter `openrouter.slug` override. When set, the public API emits a
    /// nested `openrouter: { slug: <value> }` object; NULL = unset (omitted).
    pub openrouter_slug: Option<String>,
    /// Whether the model is listed. A listed model appears in the public
    /// model catalogs (`GET /v1/models`, `GET /v1/model/list`,
    /// `GET /v1/model/{name}`) and every caller may use it. An unlisted model
    /// is left out of them and is restricted: only an organization that holds
    /// a grant may use it (see [`ModelCaller`]). Usage recording does not
    /// look at this field.
    pub is_listed: bool,
    /// When the model row was created — used as OpenRouter's `created` unix timestamp.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Who is asking for a model. Every lookup of a model for a request states
/// its caller, and the answer depends on it:
///
/// - a listed model resolves for every caller;
/// - an unlisted model resolves only for [`ModelCaller::Trusted`] and for an
///   [`ModelCaller::Organization`] that holds a grant for it.
///
/// A model that does not resolve for the caller is reported exactly like a
/// model that does not exist, so the caller cannot learn that it is there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCaller {
    /// No organization is known, as on an unauthenticated route. Listed
    /// models only.
    Anonymous,
    /// A request made on behalf of this organization. Listed models, plus the
    /// unlisted models the organization holds a grant for.
    Organization(Uuid),
    /// The platform itself: admin flows, usage recording, background jobs and
    /// lookups of a model the platform chose. Every model. Not for a model
    /// name taken from a request, unless that request has already been served
    /// to its real caller (the deprecation-header middleware).
    Trusted,
}

/// One unlisted model and the organizations that may use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestrictedModelAccess {
    /// Canonical model name.
    pub model_name: String,
    /// Active organizations that hold a grant for the model. Empty when
    /// nobody may use it.
    pub organization_ids: Vec<Uuid>,
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
///
/// The lookups here return every active model, unlisted ones included, and
/// apply no access rule. Code that looks a model up for a request must go
/// through [`ModelsServiceTrait`] or `resolve_model_cached`, which take the
/// [`ModelCaller`].
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

    /// Whether the organization holds a grant for the model that is in
    /// effect: the organization and the model must both be active.
    ///
    /// The default knows of no grants and refuses, so an implementor that
    /// does not override it cannot open an unlisted model to anyone.
    async fn organization_has_model_access(
        &self,
        _organization_id: Uuid,
        _model_id: Uuid,
    ) -> Result<bool, anyhow::Error> {
        Ok(false)
    }

    /// Every active unlisted model with the active organizations that hold a
    /// grant for it, ordered by model name.
    ///
    /// The default fails instead of returning an empty list: a reader takes a
    /// model that is absent from the list to be unrestricted.
    async fn list_restricted_model_access(
        &self,
    ) -> Result<Vec<RestrictedModelAccess>, anyhow::Error> {
        Err(anyhow::anyhow!(
            "this models repository does not list model access"
        ))
    }
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ModelsServiceTrait: Send + Sync {
    /// Get basic model info (from inference providers)
    async fn get_models(&self) -> Result<Vec<ModelInfo>, ModelsError>;

    /// Get models with pricing and metadata (from database).
    ///
    /// Every active model, unlisted ones included: this is the trusted view.
    /// Before answering a request from one of these rows, check it with
    /// [`Self::caller_can_use_model`]; public catalogs read
    /// [`Self::get_public_models_with_pricing`] instead.
    ///
    /// Implementations are expected to cache the result with a short TTL since
    /// this endpoint is publicly accessible and called frequently.
    async fn get_models_with_pricing(&self) -> Result<Vec<ModelWithPricing>, ModelsError>;

    /// The models of [`Self::get_models_with_pricing`] that are listed in the
    /// public catalogs (`is_listed`). Every unauthenticated catalog endpoint
    /// must read this one, so an unlisted model never reaches them.
    async fn get_public_models_with_pricing(&self) -> Result<Vec<ModelWithPricing>, ModelsError> {
        let mut models = self.get_models_with_pricing().await?;
        models.retain(|model| model.is_listed);
        Ok(models)
    }

    /// Get a specific model by name, for `caller`.
    ///
    /// A model the caller may not use is `NotFound`, with the error an
    /// unknown name gets.
    async fn get_model_by_name(
        &self,
        model_name: &str,
        caller: ModelCaller,
    ) -> Result<ModelWithPricing, ModelsError>;

    /// Resolve a model identifier (alias or canonical name) for `caller` and
    /// return the full model details.
    ///
    /// Returns an error if the model is not found or not active. A model the
    /// caller may not use is `NotFound`, with the error an unknown name gets.
    async fn resolve_and_get_model(
        &self,
        identifier: &str,
        caller: ModelCaller,
    ) -> Result<ModelWithPricing, ModelsError>;

    /// Whether `caller` may use `model`, a row this service returned.
    ///
    /// A listed model costs nothing to check. Only an unlisted model asked
    /// for by an organization consults the grants.
    ///
    /// The default knows of no grants: it opens an unlisted model to
    /// [`ModelCaller::Trusted`] only.
    async fn caller_can_use_model(
        &self,
        caller: ModelCaller,
        model: &ModelWithPricing,
    ) -> Result<bool, ModelsError> {
        Ok(model.is_listed || caller == ModelCaller::Trusted)
    }

    /// Resolve a model identifier for public model-detail reads. An unlisted
    /// model is answered like a model that does not exist.
    ///
    /// Production implementations should use the cached active public model
    /// list, preferring exact canonical model-name matches before alias matches.
    /// The default keeps existing test doubles source-compatible; concrete
    /// public catalog services should override it.
    async fn resolve_public_model(
        &self,
        identifier: &str,
    ) -> Result<ModelWithPricing, ModelsError> {
        let model = self
            .resolve_and_get_model(identifier, ModelCaller::Anonymous)
            .await?;
        if model.is_listed {
            Ok(model)
        } else {
            Err(ModelsError::NotFound(format!(
                "Model '{identifier}' not found"
            )))
        }
    }

    /// Resolve `identifier` against the cached active-models alias map.
    /// Returns `Some(canonical_name)` when `identifier` is a registered
    /// alias of an active model that `caller` may use, `None` when it is a
    /// canonical name, unknown, an alias of a model the caller may not use,
    /// or the lookup fails.
    ///
    /// Unlike `resolve_and_get_model`, this never hits the DB on the hot
    /// path for a listed model (cache-backed, short TTL) and is intended for
    /// advisory annotations such as alias-substitution warnings —
    /// authoritative checks must use `resolve_and_get_model`.
    async fn resolve_alias_cached(&self, identifier: &str, caller: ModelCaller) -> Option<String>;

    /// Get list of configured model names (canonical names) from database
    /// Returns only active models that have been configured with pricing
    async fn get_configured_model_names(&self) -> Result<Vec<String>, ModelsError>;

    /// Every active unlisted model with the active organizations that hold a
    /// grant for it. Trusted services (a gateway that enforces access itself)
    /// read this; a model that is absent from it is unrestricted.
    ///
    /// The default fails instead of returning an empty list, which a reader
    /// would take to mean that no model is restricted.
    async fn get_restricted_model_access(&self) -> Result<Vec<RestrictedModelAccess>, ModelsError> {
        Err(ModelsError::InternalError(
            "this models service does not list model access".to_string(),
        ))
    }

    /// Invalidate any cached model list / metadata and cached access
    /// decisions. Called by admin writes that mutate the `models`,
    /// `model_aliases` or `organization_model_access` tables.
    async fn invalidate_models_cache(&self);
}
