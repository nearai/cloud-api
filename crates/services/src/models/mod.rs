mod capabilities;
pub mod ports;
pub use capabilities::InferenceEndpoint;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moka::future::Cache;
pub use ports::{
    ModelCaller, ModelInfo, ModelWithPricing, ModelsError, ModelsRepository, ModelsServiceTrait,
    RestrictedModelAccess,
};
use tracing::warn;
use uuid::Uuid;

use crate::inference_provider_pool::{BackendModelMetadata, InferenceProviderPool};

/// TTL for the cached `/v1/model/list` response.
///
/// `/v1/model/list` is a public, unauthenticated endpoint that ran two
/// sequential DB queries (count + list with JOIN+GROUP BY) on every hit. It
/// also enriches catalog rows with backend-advertised context lengths, which
/// requires outbound provider metadata calls. With ~34 models in the system,
/// pagination is pointless and the result rarely changes, so we cache it
/// in-process for a short window and invalidate explicitly on admin writes
/// (see `invalidate_models_cache`).
const MODELS_LIST_CACHE_TTL_SECS: u64 = 300;
const BACKEND_MODEL_METADATA_FETCH_TIMEOUT_SECS: u64 = 5;

/// Capacity for the model-list cache. We only ever store one entry
/// (keyed by `"all"`), so 1 is sufficient.
const MODELS_LIST_CACHE_CAPACITY: u64 = 1;

/// Cache key used for the single model-list entry.
const MODELS_LIST_CACHE_KEY: &str = "all";

/// TTL backstop for the per-request model-resolve cache. Same-instance admin
/// writes clear it via `invalidate_models_cache` (best-effort: a resolve that
/// was in flight during the write can repopulate a stale entry, also bounded
/// by this TTL, see `resolve_model_cached`); otherwise this bounds staleness for writes made through
/// other instances.
const MODEL_RESOLVE_CACHE_TTL_SECS: u64 = 30;
const MODEL_RESOLVE_CACHE_CAPACITY: u64 = 1_000;

/// Capacity of the grant cache: one small entry per (organization, unlisted
/// model) pair that was asked for within the TTL.
const MODEL_ACCESS_CACHE_CAPACITY: u64 = 10_000;

/// The caches behind `resolve_model_cached`. Cloning shares them. Owned by
/// `ModelsServiceImpl` (which clears them on invalidation) and handed to the
/// other services that resolve a model for a request.
///
/// - `models`: positive-only cache of `ModelsRepository::resolve_and_get_model`,
///   keyed by the requested model string (alias or canonical name). It holds
///   the model itself, for any caller.
/// - `access`: cache of `ModelsRepository::organization_has_model_access`,
///   keyed by (organization id, model id). It holds both answers, so an
///   organization that keeps asking for a model it may not use does not reach
///   the database every time. Only unlisted models are ever looked up here.
///
/// Both share one TTL, so a grant or a revocation made through another
/// instance takes effect here within that time.
#[derive(Clone)]
pub struct ModelResolveCache {
    models: Cache<String, ModelWithPricing>,
    access: Cache<(Uuid, Uuid), bool>,
}

impl ModelResolveCache {
    /// Drop every cached model and every cached access decision.
    pub fn invalidate_all(&self) {
        self.models.invalidate_all();
        self.access.invalidate_all();
    }
}

pub fn new_model_resolve_cache() -> ModelResolveCache {
    model_resolve_cache_with_ttl(Duration::from_secs(MODEL_RESOLVE_CACHE_TTL_SECS))
}

pub fn model_resolve_cache_with_ttl(ttl: Duration) -> ModelResolveCache {
    ModelResolveCache {
        models: Cache::builder()
            .max_capacity(MODEL_RESOLVE_CACHE_CAPACITY)
            .time_to_live(ttl)
            .build(),
        access: Cache::builder()
            .max_capacity(MODEL_ACCESS_CACHE_CAPACITY)
            .time_to_live(ttl)
            .build(),
    }
}

/// Outcome of a failed single-flight load: not-found is not cached, errors
/// are shared with the callers coalesced onto the same load.
enum ResolveMiss {
    NotFound,
    Failed(anyhow::Error),
}

/// Resolve `identifier` (alias or canonical name) to an active model that
/// `caller` may use. This is the one place a model is resolved for a request.
///
/// `None` means "no such model for this caller": the name is unknown, or it
/// names an unlisted model the caller holds no grant for. The two cases are
/// deliberately not told apart, so callers cannot report them differently.
///
/// The model comes from `ModelsRepository::resolve_and_get_model` through the
/// positive-only cache. Concurrent misses for the same identifier are coalesced
/// into one repository read (moka single-flight). Only found models are cached;
/// `None` is surfaced as an `Err` to moka so it is never stored and always
/// re-reads, so a newly activated model works immediately.
///
/// Invalidation: `invalidate_all` only discards entries inserted before it, so a
/// load already in flight when an admin write invalidates can still insert its
/// pre-write value afterwards. That staleness is bounded by the TTL; moka has no
/// built-in way to make an in-flight load respect invalidation. The same holds
/// for a grant lookup in flight while a grant is made or revoked.
pub async fn resolve_model_cached(
    cache: &ModelResolveCache,
    repository: &dyn ModelsRepository,
    identifier: &str,
    caller: ModelCaller,
) -> Result<Option<ModelWithPricing>, anyhow::Error> {
    let loaded = cache
        .models
        .try_get_with_by_ref(identifier, async {
            match repository.resolve_and_get_model(identifier).await {
                Ok(Some(model)) => Ok(model),
                Ok(None) => Err(ResolveMiss::NotFound),
                Err(e) => Err(ResolveMiss::Failed(e)),
            }
        })
        .await;
    let model = match loaded {
        Ok(model) => model,
        Err(miss) => match &*miss {
            ResolveMiss::NotFound => return Ok(None),
            ResolveMiss::Failed(e) => return Err(anyhow::anyhow!("{e:#}")),
        },
    };
    if caller_can_use_model(cache, repository, caller, &model).await? {
        Ok(Some(model))
    } else {
        Ok(None)
    }
}

/// The access rule. A listed model is open to every caller and returns before
/// any lookup, so it costs nothing here. An unlisted model is open to a
/// trusted caller, closed to an anonymous one, and open to an organization
/// only if it holds a grant (cached, both answers).
///
/// A failed grant lookup is an error, never an allow.
async fn caller_can_use_model(
    cache: &ModelResolveCache,
    repository: &dyn ModelsRepository,
    caller: ModelCaller,
    model: &ModelWithPricing,
) -> Result<bool, anyhow::Error> {
    if model.is_listed {
        return Ok(true);
    }
    match caller {
        ModelCaller::Trusted => Ok(true),
        ModelCaller::Anonymous => Ok(false),
        ModelCaller::Organization(organization_id) => cache
            .access
            .try_get_with((organization_id, model.id), async {
                repository
                    .organization_has_model_access(organization_id, model.id)
                    .await
            })
            .await
            .map_err(|e: Arc<anyhow::Error>| anyhow::anyhow!("{e:#}")),
    }
}

fn apply_backend_model_metadata(
    models: &mut [ModelWithPricing],
    metadata_by_model: &HashMap<String, BackendModelMetadata>,
) {
    for model in models {
        if let Some(metadata) = metadata_by_model.get(&model.model_name) {
            if let Some(context_length) = metadata.context_length.filter(|value| *value > 0) {
                model.context_length = context_length;
            }
            if let Some(max_output_length) = metadata.max_output_length.filter(|value| *value > 0) {
                model.max_output_length = Some(max_output_length);
            }
        }
    }
}

async fn backend_model_metadata_with_timeout<F>(
    fetch: F,
    timeout: Duration,
) -> HashMap<String, BackendModelMetadata>
where
    F: Future<Output = HashMap<String, BackendModelMetadata>>,
{
    match tokio::time::timeout(timeout, fetch).await {
        Ok(metadata_by_model) => metadata_by_model,
        Err(_) => {
            warn!("Timed out fetching backend model metadata");
            HashMap::new()
        }
    }
}

pub struct ModelsServiceImpl {
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub models_repository: Arc<dyn ModelsRepository>,
    /// In-process cache for the full model list. Keyed by a static
    /// sentinel since pagination has been dropped — there is only ever one
    /// list to serve.
    models_list_cache: Cache<&'static str, Arc<Vec<ModelWithPricing>>>,
    /// Shared with the completion and attestation services; cleared by
    /// `invalidate_models_cache`.
    model_resolve_cache: ModelResolveCache,
}

impl ModelsServiceImpl {
    pub fn new(
        inference_provider_pool: Arc<InferenceProviderPool>,
        models_repository: Arc<dyn ModelsRepository>,
    ) -> Self {
        let models_list_cache = Cache::builder()
            .max_capacity(MODELS_LIST_CACHE_CAPACITY)
            .time_to_live(Duration::from_secs(MODELS_LIST_CACHE_TTL_SECS))
            .build();
        Self {
            inference_provider_pool,
            models_repository,
            models_list_cache,
            model_resolve_cache: new_model_resolve_cache(),
        }
    }

    /// Handle to the resolve cache for the other services that resolve a
    /// model for a request. Invalidation on this service clears it, so admin
    /// writes take effect immediately here.
    pub fn model_resolve_cache(&self) -> ModelResolveCache {
        self.model_resolve_cache.clone()
    }

    /// Fetch the active-models list through the in-process cache, returning
    /// the shared `Arc` so callers that only need to scan the list (e.g.
    /// alias resolution) avoid cloning every entry.
    ///
    /// Uses `try_get_with` to coalesce concurrent loads: when the cache is
    /// empty (cold start, after TTL expiry, or after an admin invalidation),
    /// moka guarantees that only ONE caller runs the async loader and any
    /// other callers waiting on the same key receive the same result.
    /// Without this, every cache miss would let N concurrent requests all
    /// hit the DB with the same JOIN+GROUP BY query — defeating most of
    /// the cache win and producing periodic spikes every 5 min.
    async fn cached_models(&self) -> Result<Arc<Vec<ModelWithPricing>>, ModelsError> {
        let repo = self.models_repository.clone();
        let inference_provider_pool = self.inference_provider_pool.clone();
        self.models_list_cache
            .try_get_with(MODELS_LIST_CACHE_KEY, async move {
                let mut models = repo
                    .get_all_active_models()
                    .await
                    .map_err(|e| ModelsError::InternalError(e.to_string()))?;
                let metadata_by_model = backend_model_metadata_with_timeout(
                    inference_provider_pool.max_model_metadata_by_model(),
                    Duration::from_secs(BACKEND_MODEL_METADATA_FETCH_TIMEOUT_SECS),
                )
                .await;
                apply_backend_model_metadata(&mut models, &metadata_by_model);
                Ok(Arc::new(models))
            })
            .await
            .map_err(|e: Arc<ModelsError>| ModelsError::InternalError(e.to_string()))
    }
}

#[async_trait]
impl ModelsServiceTrait for ModelsServiceImpl {
    async fn get_models(&self) -> Result<Vec<ModelInfo>, ModelsError> {
        let names = self.inference_provider_pool.registered_model_names().await;
        Ok(names
            .into_iter()
            .map(|name| ModelInfo {
                created: 0,
                id: name,
                object: "model".to_string(),
                owned_by: "system".to_string(),
            })
            .collect())
    }

    async fn get_models_with_pricing(&self) -> Result<Vec<ModelWithPricing>, ModelsError> {
        let arc = self.cached_models().await?;
        Ok((*arc).clone())
    }

    async fn get_model_by_name(
        &self,
        model_name: &str,
        caller: ModelCaller,
    ) -> Result<ModelWithPricing, ModelsError> {
        let not_found = || ModelsError::NotFound(format!("Model '{model_name}' not found"));
        let model = self
            .models_repository
            .get_model_by_name(model_name)
            .await
            .map_err(|e| ModelsError::InternalError(e.to_string()))?
            .ok_or_else(not_found)?;
        // A model the caller may not use gets the error of an unknown name.
        if self.caller_can_use_model(caller, &model).await? {
            Ok(model)
        } else {
            Err(not_found())
        }
    }

    async fn resolve_and_get_model(
        &self,
        identifier: &str,
        caller: ModelCaller,
    ) -> Result<ModelWithPricing, ModelsError> {
        resolve_model_cached(
            &self.model_resolve_cache,
            self.models_repository.as_ref(),
            identifier,
            caller,
        )
        .await
        .map_err(|e| ModelsError::InternalError(e.to_string()))?
        .ok_or_else(|| ModelsError::NotFound(format!("Model '{identifier}' not found")))
    }

    async fn caller_can_use_model(
        &self,
        caller: ModelCaller,
        model: &ModelWithPricing,
    ) -> Result<bool, ModelsError> {
        caller_can_use_model(
            &self.model_resolve_cache,
            self.models_repository.as_ref(),
            caller,
            model,
        )
        .await
        .map_err(|e| ModelsError::InternalError(e.to_string()))
    }

    async fn resolve_public_model(
        &self,
        identifier: &str,
    ) -> Result<ModelWithPricing, ModelsError> {
        let models = self.cached_models().await?;
        // An unlisted model is answered like a model that does not exist.
        let mut listed = models.iter().filter(|model| model.is_listed);
        if let Some(model) = listed.clone().find(|model| model.model_name == identifier) {
            return Ok(model.clone());
        }
        listed
            .find(|model| model.aliases.iter().any(|alias| alias == identifier))
            .cloned()
            .ok_or_else(|| ModelsError::NotFound(format!("Model '{identifier}' not found")))
    }

    async fn resolve_alias_cached(&self, identifier: &str, caller: ModelCaller) -> Option<String> {
        let models = self.cached_models().await.ok()?;
        let model = models
            .iter()
            .find(|m| m.aliases.iter().any(|a| a == identifier))?;
        // The canonical name of a model the caller may not use stays hidden.
        // A failed check hides it too.
        self.caller_can_use_model(caller, model)
            .await
            .unwrap_or(false)
            .then(|| model.model_name.clone())
    }

    async fn get_configured_model_names(&self) -> Result<Vec<String>, ModelsError> {
        self.models_repository
            .get_configured_model_names()
            .await
            .map_err(|e| ModelsError::InternalError(e.to_string()))
    }

    async fn get_restricted_model_access(&self) -> Result<Vec<RestrictedModelAccess>, ModelsError> {
        self.models_repository
            .list_restricted_model_access()
            .await
            .map_err(|e| ModelsError::InternalError(e.to_string()))
    }

    async fn invalidate_models_cache(&self) {
        self.models_list_cache.invalidate_all();
        self.model_resolve_cache.invalidate_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference_provider_pool::BackendModelMetadata;
    use config::ExternalProvidersConfig;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use uuid::Uuid;

    struct StaticModelsRepository {
        active_models: Vec<ModelWithPricing>,
        models_by_name: HashMap<String, ModelWithPricing>,
        resolved_models: HashMap<String, ModelWithPricing>,
        /// (organization id, model id) pairs that hold a grant.
        grants: Mutex<HashSet<(Uuid, Uuid)>>,
        /// Number of `organization_has_model_access` reads.
        access_lookups: AtomicUsize,
        fail_access_lookups: AtomicBool,
    }

    impl StaticModelsRepository {
        fn with_active_models(active_models: Vec<ModelWithPricing>) -> Self {
            let models_by_name = active_models
                .iter()
                .map(|model| (model.model_name.clone(), model.clone()))
                .collect();
            let resolved_models = active_models
                .iter()
                .map(|model| (model.model_name.clone(), model.clone()))
                .collect();
            Self {
                active_models,
                models_by_name,
                resolved_models,
                grants: Mutex::new(HashSet::new()),
                access_lookups: AtomicUsize::new(0),
                fail_access_lookups: AtomicBool::new(false),
            }
        }

        fn with_resolved_model(mut self, identifier: &str, model: ModelWithPricing) -> Self {
            self.resolved_models.insert(identifier.to_string(), model);
            self
        }

        fn grant(&self, organization_id: Uuid, model_id: Uuid) {
            self.grants
                .lock()
                .unwrap()
                .insert((organization_id, model_id));
        }

        fn revoke(&self, organization_id: Uuid, model_id: Uuid) {
            self.grants
                .lock()
                .unwrap()
                .remove(&(organization_id, model_id));
        }

        fn access_lookups(&self) -> usize {
            self.access_lookups.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ModelsRepository for StaticModelsRepository {
        async fn get_all_active_models(&self) -> Result<Vec<ModelWithPricing>, anyhow::Error> {
            Ok(self.active_models.clone())
        }

        async fn get_model_by_name(
            &self,
            model_name: &str,
        ) -> Result<Option<ModelWithPricing>, anyhow::Error> {
            Ok(self.models_by_name.get(model_name).cloned())
        }

        async fn resolve_and_get_model(
            &self,
            identifier: &str,
        ) -> Result<Option<ModelWithPricing>, anyhow::Error> {
            Ok(self.resolved_models.get(identifier).cloned())
        }

        async fn get_configured_model_names(&self) -> Result<Vec<String>, anyhow::Error> {
            Ok(self.models_by_name.keys().cloned().collect())
        }

        async fn organization_has_model_access(
            &self,
            organization_id: Uuid,
            model_id: Uuid,
        ) -> Result<bool, anyhow::Error> {
            self.access_lookups.fetch_add(1, Ordering::SeqCst);
            if self.fail_access_lookups.load(Ordering::SeqCst) {
                anyhow::bail!("db down");
            }
            Ok(self
                .grants
                .lock()
                .unwrap()
                .contains(&(organization_id, model_id)))
        }

        async fn list_restricted_model_access(
            &self,
        ) -> Result<Vec<RestrictedModelAccess>, anyhow::Error> {
            let grants = self.grants.lock().unwrap();
            Ok(self
                .active_models
                .iter()
                .filter(|model| !model.is_listed)
                .map(|model| RestrictedModelAccess {
                    model_name: model.model_name.clone(),
                    organization_ids: grants
                        .iter()
                        .filter(|(_, model_id)| *model_id == model.id)
                        .map(|(organization_id, _)| *organization_id)
                        .collect(),
                })
                .collect())
        }
    }

    /// A repository that keeps the trait defaults for grants.
    struct NoGrantsRepository(ModelWithPricing);

    #[async_trait]
    impl ModelsRepository for NoGrantsRepository {
        async fn get_all_active_models(&self) -> Result<Vec<ModelWithPricing>, anyhow::Error> {
            Ok(vec![self.0.clone()])
        }

        async fn get_model_by_name(
            &self,
            model_name: &str,
        ) -> Result<Option<ModelWithPricing>, anyhow::Error> {
            Ok((model_name == self.0.model_name).then(|| self.0.clone()))
        }

        async fn resolve_and_get_model(
            &self,
            identifier: &str,
        ) -> Result<Option<ModelWithPricing>, anyhow::Error> {
            Ok((identifier == self.0.model_name).then(|| self.0.clone()))
        }

        async fn get_configured_model_names(&self) -> Result<Vec<String>, anyhow::Error> {
            Ok(vec![self.0.model_name.clone()])
        }
    }

    /// A service over `repository` with no backends, for the access tests.
    fn service_over(repository: Arc<dyn ModelsRepository>) -> ModelsServiceImpl {
        ModelsServiceImpl::new(
            Arc::new(InferenceProviderPool::new(
                None,
                ExternalProvidersConfig::default(),
            )),
            repository,
        )
    }

    const LISTED: &str = "test/listed";
    const UNLISTED: &str = "test/unlisted";
    const UNLISTED_ALIAS: &str = "unlisted-alias";

    /// One listed and one unlisted model (with an alias), and their service.
    fn access_fixture() -> (Arc<StaticModelsRepository>, ModelsServiceImpl, Uuid) {
        let mut unlisted = test_catalog_model(UNLISTED);
        unlisted.is_listed = false;
        unlisted.aliases = vec![UNLISTED_ALIAS.to_string()];
        let unlisted_id = unlisted.id;
        let repository = Arc::new(
            StaticModelsRepository::with_active_models(vec![
                test_catalog_model(LISTED),
                unlisted.clone(),
            ])
            .with_resolved_model(UNLISTED_ALIAS, unlisted),
        );
        let service = service_over(repository.clone());
        (repository, service, unlisted_id)
    }

    fn test_catalog_model(model_name: &str) -> ModelWithPricing {
        test_catalog_model_with_output(model_name, Some(1024))
    }

    #[test]
    fn systemone_endpoint_compatibility_preserves_other_modalities() {
        let mut model = test_catalog_model("fixture");
        for modalities in [
            None,
            Some(vec![]),
            Some(vec!["text".into()]),
            Some(vec!["image".into()]),
            Some(vec!["text".into(), "audio".into()]),
        ] {
            model.output_modalities = modalities;
            assert!(model
                .validate_endpoint(InferenceEndpoint::ChatCompletions)
                .is_ok());
            assert!(model
                .validate_endpoint(InferenceEndpoint::Responses)
                .is_ok());
            assert!(model
                .validate_endpoint(InferenceEndpoint::SystemOne)
                .is_err());
        }
        model.output_modalities = Some(vec!["decisions".into()]);
        assert!(model
            .validate_endpoint(InferenceEndpoint::ChatCompletions)
            .is_err());
        assert!(model
            .validate_endpoint(InferenceEndpoint::Responses)
            .is_err());
        assert!(model
            .validate_endpoint(InferenceEndpoint::SystemOne)
            .is_ok());
    }

    fn test_catalog_model_with_output(
        model_name: &str,
        max_output_length: Option<i32>,
    ) -> ModelWithPricing {
        ModelWithPricing {
            id: Uuid::new_v4(),
            model_name: model_name.to_string(),
            model_display_name: model_name.to_string(),
            model_description: "test model".to_string(),
            model_icon: None,
            input_cost_per_token: 1,
            output_cost_per_token: 1,
            cost_per_image: 0,
            cache_read_cost_per_token: None,
            text_pricing: None,
            context_length: 4096,
            verifiable: true,
            aliases: Vec::new(),
            owned_by: "near".to_string(),
            provider_type: "vllm".to_string(),
            provider_config: None,
            attestation_supported: true,
            input_modalities: Some(vec!["text".to_string()]),
            output_modalities: Some(vec!["text".to_string()]),
            inference_url: Some("mock://near".to_string()),
            hugging_face_id: None,
            quantization: None,
            max_output_length,
            supported_sampling_parameters: Vec::new(),
            supported_features: Vec::new(),
            datacenters: None,
            is_ready: None,
            deprecation_date: None,
            successor_model_name: None,
            openrouter_slug: None,
            is_listed: true,
            created_at: chrono::Utc::now(),
        }
    }

    fn provider_model(
        model_name: &str,
        context_length: Option<i32>,
        max_output_length: Option<i32>,
    ) -> inference_providers::ModelInfo {
        inference_providers::ModelInfo {
            id: model_name.to_string(),
            object: "model".to_string(),
            created: 0,
            owned_by: "test".to_string(),
            context_length,
            max_model_len: None,
            max_output_length,
            top_provider: None,
        }
    }

    async fn service_with_backend_models(
        repository: StaticModelsRepository,
        model_name: &str,
        backend_models: Vec<inference_providers::ModelInfo>,
    ) -> ModelsServiceImpl {
        let pool = Arc::new(InferenceProviderPool::new(
            None,
            ExternalProvidersConfig::default(),
        ));
        pool.register_provider(
            model_name.to_string(),
            Arc::new(inference_providers::mock::MockProvider::with_models(
                backend_models,
            )),
        )
        .await;
        ModelsServiceImpl::new(pool, Arc::new(repository))
    }

    #[tokio::test]
    async fn get_models_with_pricing_backend_model_metadata_with_timeout_returns_values_before_deadline(
    ) {
        let result = backend_model_metadata_with_timeout(
            async {
                HashMap::from([(
                    "test/model".to_string(),
                    BackendModelMetadata {
                        context_length: Some(65_536),
                        max_output_length: Some(8_192),
                    },
                )])
            },
            Duration::from_secs(1),
        )
        .await;

        assert_eq!(
            result.get("test/model"),
            Some(&BackendModelMetadata {
                context_length: Some(65_536),
                max_output_length: Some(8_192),
            })
        );
    }

    #[tokio::test]
    async fn get_models_with_pricing_backend_model_metadata_with_timeout_returns_empty_after_deadline(
    ) {
        let result = backend_model_metadata_with_timeout(
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                HashMap::from([(
                    "test/model".to_string(),
                    BackendModelMetadata {
                        context_length: Some(65_536),
                        max_output_length: Some(8_192),
                    },
                )])
            },
            Duration::from_millis(1),
        )
        .await;

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn get_models_with_pricing_uses_backend_context_and_output_when_positive() {
        let model_name = "test/model";
        let service = service_with_backend_models(
            StaticModelsRepository::with_active_models(vec![test_catalog_model(model_name)]),
            model_name,
            vec![provider_model(model_name, Some(32_768), Some(4_096))],
        )
        .await;

        let models = service.get_models_with_pricing().await.unwrap();

        assert_eq!(models[0].context_length, 32_768);
        assert_eq!(models[0].max_output_length, Some(4_096));
    }

    #[tokio::test]
    async fn get_models_with_pricing_preserves_db_output_when_backend_output_missing() {
        let model_name = "test/model";
        let service = service_with_backend_models(
            StaticModelsRepository::with_active_models(vec![test_catalog_model(model_name)]),
            model_name,
            vec![provider_model(model_name, Some(32_768), None)],
        )
        .await;

        let models = service.get_models_with_pricing().await.unwrap();

        assert_eq!(models[0].context_length, 32_768);
        assert_eq!(models[0].max_output_length, Some(1024));
    }

    #[tokio::test]
    async fn get_models_with_pricing_uses_backend_output_when_db_output_missing() {
        let model_name = "test/model";
        let service = service_with_backend_models(
            StaticModelsRepository::with_active_models(vec![test_catalog_model_with_output(
                model_name, None,
            )]),
            model_name,
            vec![provider_model(model_name, Some(32_768), Some(4_096))],
        )
        .await;

        let models = service.get_models_with_pricing().await.unwrap();

        assert_eq!(models[0].max_output_length, Some(4_096));
    }

    #[tokio::test]
    async fn get_models_with_pricing_ignores_zero_and_negative_backend_output() {
        let model_name = "test/model";
        let service = service_with_backend_models(
            StaticModelsRepository::with_active_models(vec![test_catalog_model(model_name)]),
            model_name,
            vec![provider_model(model_name, Some(32_768), Some(-1))],
        )
        .await;

        let models = service.get_models_with_pricing().await.unwrap();

        assert_eq!(models[0].max_output_length, Some(1024));
    }

    #[tokio::test]
    async fn get_models_with_pricing_caches_backend_output_metadata_until_invalidation() {
        let model_name = "test/model";
        let pool = Arc::new(InferenceProviderPool::new(
            None,
            ExternalProvidersConfig::default(),
        ));
        pool.register_provider(
            model_name.to_string(),
            Arc::new(inference_providers::mock::MockProvider::with_models(vec![
                provider_model(model_name, Some(32_768), Some(4_096)),
            ])),
        )
        .await;
        let service = ModelsServiceImpl::new(
            pool.clone(),
            Arc::new(StaticModelsRepository::with_active_models(vec![
                test_catalog_model(model_name),
            ])),
        );

        let first = service.get_models_with_pricing().await.unwrap();
        assert_eq!(first[0].context_length, 32_768);
        assert_eq!(first[0].max_output_length, Some(4_096));

        pool.register_provider(
            model_name.to_string(),
            Arc::new(inference_providers::mock::MockProvider::with_models(vec![
                provider_model(model_name, Some(65_536), Some(8_192)),
            ])),
        )
        .await;

        let cached = service.get_models_with_pricing().await.unwrap();
        assert_eq!(cached[0].context_length, 32_768);
        assert_eq!(cached[0].max_output_length, Some(4_096));

        service.invalidate_models_cache().await;
        let refreshed = service.get_models_with_pricing().await.unwrap();
        assert_eq!(refreshed[0].context_length, 65_536);
        assert_eq!(refreshed[0].max_output_length, Some(8_192));
    }

    #[tokio::test]
    async fn get_models_with_pricing_public_resolver_uses_enriched_canonical_and_alias_lookup() {
        let model_name = "test/model";
        let mut catalog_model = test_catalog_model(model_name);
        catalog_model.aliases = vec!["friendly".to_string()];
        let service = service_with_backend_models(
            StaticModelsRepository::with_active_models(vec![catalog_model]),
            model_name,
            vec![provider_model(model_name, Some(32_768), Some(4_096))],
        )
        .await;

        let canonical = service.resolve_public_model(model_name).await.unwrap();
        let alias = service.resolve_public_model("friendly").await.unwrap();

        assert_eq!(canonical.context_length, 32_768);
        assert_eq!(canonical.max_output_length, Some(4_096));
        assert_eq!(alias.model_name, model_name);
        assert_eq!(alias.max_output_length, Some(4_096));
    }

    #[tokio::test]
    async fn unlisted_models_stay_out_of_the_public_views() {
        let listed_name = "test/listed";
        let unlisted_name = "test/unlisted";
        let mut unlisted = test_catalog_model(unlisted_name);
        unlisted.is_listed = false;
        unlisted.aliases = vec!["unlisted-alias".to_string()];
        let service = service_with_backend_models(
            StaticModelsRepository::with_active_models(vec![
                test_catalog_model(listed_name),
                unlisted,
            ]),
            listed_name,
            vec![provider_model(listed_name, Some(32_768), Some(4_096))],
        )
        .await;

        let all = service.get_models_with_pricing().await.unwrap();
        assert_eq!(all.len(), 2);

        let public = service.get_public_models_with_pricing().await.unwrap();
        assert_eq!(public.len(), 1);
        assert_eq!(public[0].model_name, listed_name);

        assert!(service.resolve_public_model(listed_name).await.is_ok());
        for identifier in [unlisted_name, "unlisted-alias"] {
            assert!(matches!(
                service.resolve_public_model(identifier).await,
                Err(ModelsError::NotFound(_))
            ));
        }

        // A trusted caller (usage recording, admin flows) still resolves it.
        let resolved = service
            .resolve_and_get_model(unlisted_name, ModelCaller::Trusted)
            .await
            .unwrap();
        assert_eq!(resolved.model_name, unlisted_name);
        assert_eq!(
            service
                .resolve_alias_cached("unlisted-alias", ModelCaller::Trusted)
                .await,
            Some(unlisted_name.to_string())
        );
    }

    #[tokio::test]
    async fn a_listed_model_is_open_to_every_caller_and_never_consults_grants() {
        let (repository, service, _) = access_fixture();

        for caller in [
            ModelCaller::Anonymous,
            ModelCaller::Organization(Uuid::new_v4()),
            ModelCaller::Trusted,
        ] {
            let resolved = service.resolve_and_get_model(LISTED, caller).await.unwrap();
            assert_eq!(resolved.model_name, LISTED);
            let by_name = service.get_model_by_name(LISTED, caller).await.unwrap();
            assert_eq!(by_name.model_name, LISTED);
            assert!(service
                .caller_can_use_model(caller, &by_name)
                .await
                .unwrap());
        }
        assert_eq!(repository.access_lookups(), 0);
    }

    #[tokio::test]
    async fn an_unlisted_model_resolves_only_for_a_trusted_caller_or_a_granted_organization() {
        let (repository, service, unlisted_id) = access_fixture();
        let granted = Uuid::new_v4();
        let other = Uuid::new_v4();
        repository.grant(granted, unlisted_id);
        // A grant on another model opens nothing here.
        repository.grant(other, Uuid::new_v4());

        for caller in [ModelCaller::Trusted, ModelCaller::Organization(granted)] {
            for identifier in [UNLISTED, UNLISTED_ALIAS] {
                let resolved = service
                    .resolve_and_get_model(identifier, caller)
                    .await
                    .unwrap();
                assert_eq!(resolved.model_name, UNLISTED, "{caller:?} {identifier}");
            }
            let by_name = service.get_model_by_name(UNLISTED, caller).await.unwrap();
            assert!(service
                .caller_can_use_model(caller, &by_name)
                .await
                .unwrap());
            assert_eq!(
                service.resolve_alias_cached(UNLISTED_ALIAS, caller).await,
                Some(UNLISTED.to_string())
            );
        }

        let unlisted = service
            .get_model_by_name(UNLISTED, ModelCaller::Trusted)
            .await
            .unwrap();
        for caller in [ModelCaller::Anonymous, ModelCaller::Organization(other)] {
            for identifier in [UNLISTED, UNLISTED_ALIAS] {
                assert!(
                    matches!(
                        service.resolve_and_get_model(identifier, caller).await,
                        Err(ModelsError::NotFound(_))
                    ),
                    "{caller:?} {identifier}"
                );
            }
            assert!(matches!(
                service.get_model_by_name(UNLISTED, caller).await,
                Err(ModelsError::NotFound(_))
            ));
            assert!(!service
                .caller_can_use_model(caller, &unlisted)
                .await
                .unwrap());
            // The alias does not disclose the canonical name either.
            assert_eq!(
                service.resolve_alias_cached(UNLISTED_ALIAS, caller).await,
                None
            );
        }
    }

    #[tokio::test]
    async fn a_refused_model_gets_the_error_of_an_unknown_one() {
        let (_, service, _) = access_fixture();
        let caller = ModelCaller::Organization(Uuid::new_v4());
        let unknown = "test/no-such-model";

        let refused = service
            .resolve_and_get_model(UNLISTED, caller)
            .await
            .unwrap_err();
        let missing = service
            .resolve_and_get_model(unknown, caller)
            .await
            .unwrap_err();
        assert!(matches!(refused, ModelsError::NotFound(_)));
        assert!(matches!(missing, ModelsError::NotFound(_)));
        assert_eq!(
            refused.to_string().replace(UNLISTED, unknown),
            missing.to_string()
        );

        let refused = service
            .get_model_by_name(UNLISTED, caller)
            .await
            .unwrap_err();
        let missing = service
            .get_model_by_name(unknown, caller)
            .await
            .unwrap_err();
        assert!(matches!(refused, ModelsError::NotFound(_)));
        assert!(matches!(missing, ModelsError::NotFound(_)));
        assert_eq!(
            refused.to_string().replace(UNLISTED, unknown),
            missing.to_string()
        );
    }

    #[tokio::test]
    async fn access_decisions_are_cached_both_ways_and_cleared_by_invalidation() {
        let (repository, service, unlisted_id) = access_fixture();
        let organization = Uuid::new_v4();
        let caller = ModelCaller::Organization(organization);
        let resolves = || async {
            service
                .resolve_and_get_model(UNLISTED, caller)
                .await
                .is_ok()
        };

        // Refused, and the refusal is cached: one grant lookup for three asks.
        for _ in 0..3 {
            assert!(!resolves().await);
        }
        assert_eq!(repository.access_lookups(), 1);

        // A grant written behind the cache's back is not seen...
        repository.grant(organization, unlisted_id);
        assert!(!resolves().await);
        assert_eq!(repository.access_lookups(), 1);
        // ...until the cache is invalidated, as granting through the admin
        // API does.
        service.invalidate_models_cache().await;
        for _ in 0..3 {
            assert!(resolves().await);
        }
        assert_eq!(repository.access_lookups(), 2);

        // The same for a revocation.
        repository.revoke(organization, unlisted_id);
        assert!(resolves().await);
        service.invalidate_models_cache().await;
        assert!(!resolves().await);
        assert_eq!(repository.access_lookups(), 3);

        // Decisions are per organization.
        assert!(service
            .resolve_and_get_model(UNLISTED, ModelCaller::Organization(Uuid::new_v4()))
            .await
            .is_err());
        assert_eq!(repository.access_lookups(), 4);
    }

    #[tokio::test]
    async fn access_decisions_expire_with_the_resolve_cache_ttl() {
        let (repository, _, unlisted_id) = access_fixture();
        let cache = model_resolve_cache_with_ttl(Duration::from_millis(50));
        let organization = Uuid::new_v4();
        let caller = ModelCaller::Organization(organization);

        assert!(
            resolve_model_cached(&cache, repository.as_ref(), UNLISTED, caller)
                .await
                .unwrap()
                .is_none()
        );
        repository.grant(organization, unlisted_id);
        tokio::time::sleep(Duration::from_millis(150)).await;
        // No invalidation: this is how another instance learns of the grant.
        assert!(
            resolve_model_cached(&cache, repository.as_ref(), UNLISTED, caller)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(repository.access_lookups(), 2);
    }

    #[tokio::test]
    async fn a_failed_grant_lookup_is_an_error_and_is_not_cached() {
        let (repository, service, unlisted_id) = access_fixture();
        let organization = Uuid::new_v4();
        let caller = ModelCaller::Organization(organization);
        repository.grant(organization, unlisted_id);

        repository.fail_access_lookups.store(true, Ordering::SeqCst);
        assert!(matches!(
            service.resolve_and_get_model(UNLISTED, caller).await,
            Err(ModelsError::InternalError(_))
        ));
        assert!(matches!(
            service.get_model_by_name(UNLISTED, caller).await,
            Err(ModelsError::InternalError(_))
        ));
        // The advisory alias lookup hides the model rather than failing.
        assert_eq!(
            service.resolve_alias_cached(UNLISTED_ALIAS, caller).await,
            None
        );

        repository
            .fail_access_lookups
            .store(false, Ordering::SeqCst);
        assert!(service
            .resolve_and_get_model(UNLISTED, caller)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_repository_without_grants_opens_an_unlisted_model_to_trusted_callers_only() {
        let mut unlisted = test_catalog_model(UNLISTED);
        unlisted.is_listed = false;
        let service = service_over(Arc::new(NoGrantsRepository(unlisted)));

        assert!(service
            .resolve_and_get_model(UNLISTED, ModelCaller::Trusted)
            .await
            .is_ok());
        for caller in [
            ModelCaller::Anonymous,
            ModelCaller::Organization(Uuid::new_v4()),
        ] {
            assert!(matches!(
                service.resolve_and_get_model(UNLISTED, caller).await,
                Err(ModelsError::NotFound(_))
            ));
        }
        // It cannot say which models are restricted, so it must not claim
        // that none is.
        assert!(service.get_restricted_model_access().await.is_err());
    }

    #[tokio::test]
    async fn restricted_model_access_lists_unlisted_models_with_their_organizations() {
        let (repository, service, unlisted_id) = access_fixture();

        let access = service.get_restricted_model_access().await.unwrap();
        assert_eq!(
            access,
            vec![RestrictedModelAccess {
                model_name: UNLISTED.to_string(),
                organization_ids: Vec::new(),
            }]
        );

        let organization = Uuid::new_v4();
        repository.grant(organization, unlisted_id);
        let access = service.get_restricted_model_access().await.unwrap();
        assert_eq!(access[0].organization_ids, vec![organization]);
    }

    #[tokio::test]
    async fn get_models_with_pricing_public_resolver_exact_model_name_wins_over_alias() {
        let aliased_model_name = "test/aliased";
        let exact_model_name = "friendly";
        let mut aliased_model = test_catalog_model(aliased_model_name);
        aliased_model.aliases = vec![exact_model_name.to_string()];
        let exact_model = test_catalog_model(exact_model_name);
        let repository =
            StaticModelsRepository::with_active_models(vec![aliased_model.clone(), exact_model])
                .with_resolved_model(exact_model_name, aliased_model);
        let service = service_with_backend_models(
            repository,
            aliased_model_name,
            vec![provider_model(
                aliased_model_name,
                Some(32_768),
                Some(4_096),
            )],
        )
        .await;

        let resolved = service
            .resolve_public_model(exact_model_name)
            .await
            .unwrap();

        assert_eq!(resolved.model_name, exact_model_name);
    }

    #[tokio::test]
    async fn get_models_with_pricing_public_resolver_does_not_return_inactive_db_fallback() {
        let inactive_model_name = "test/inactive";
        let repository = StaticModelsRepository::with_active_models(Vec::new())
            .with_resolved_model(inactive_model_name, test_catalog_model(inactive_model_name));
        let service = service_with_backend_models(
            repository,
            inactive_model_name,
            vec![provider_model(
                inactive_model_name,
                Some(32_768),
                Some(4_096),
            )],
        )
        .await;

        let result = service.resolve_public_model(inactive_model_name).await;

        assert!(matches!(result, Err(ModelsError::NotFound(_))));
    }

    #[tokio::test]
    async fn get_models_with_pricing_db_reads_remain_canonical_and_unenriched() {
        let model_name = "test/model";
        let mut catalog_model = test_catalog_model(model_name);
        catalog_model.aliases = vec!["friendly".to_string()];
        let repository = StaticModelsRepository::with_active_models(vec![catalog_model.clone()])
            .with_resolved_model("friendly", catalog_model);
        let service = service_with_backend_models(
            repository,
            model_name,
            vec![provider_model(model_name, Some(32_768), Some(4_096))],
        )
        .await;

        let caller = ModelCaller::Anonymous;
        let by_name = service.get_model_by_name(model_name, caller).await.unwrap();
        let by_alias = service.get_model_by_name("friendly", caller).await;
        let resolved_alias = service
            .resolve_and_get_model("friendly", caller)
            .await
            .unwrap();

        assert_eq!(by_name.context_length, 4096);
        assert_eq!(by_name.max_output_length, Some(1024));
        assert!(matches!(by_alias, Err(ModelsError::NotFound(_))));
        assert_eq!(resolved_alias.context_length, 4096);
        assert_eq!(resolved_alias.max_output_length, Some(1024));
    }
}
