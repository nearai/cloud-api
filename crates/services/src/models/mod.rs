pub mod ports;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moka::future::Cache;
pub use ports::{ModelInfo, ModelWithPricing, ModelsError, ModelsRepository, ModelsServiceTrait};
use tracing::warn;

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
        }
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

    async fn get_model_by_name(&self, model_name: &str) -> Result<ModelWithPricing, ModelsError> {
        self.models_repository
            .get_model_by_name(model_name)
            .await
            .map_err(|e| ModelsError::InternalError(e.to_string()))?
            .ok_or_else(|| ModelsError::NotFound(format!("Model '{model_name}' not found")))
    }

    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> Result<ModelWithPricing, ModelsError> {
        self.models_repository
            .resolve_and_get_model(identifier)
            .await
            .map_err(|e| ModelsError::InternalError(e.to_string()))?
            .ok_or_else(|| ModelsError::NotFound(format!("Model '{identifier}' not found")))
    }

    async fn resolve_public_model(
        &self,
        identifier: &str,
    ) -> Result<ModelWithPricing, ModelsError> {
        let models = self.cached_models().await?;
        if let Some(model) = models.iter().find(|model| model.model_name == identifier) {
            return Ok(model.clone());
        }
        models
            .iter()
            .find(|model| model.aliases.iter().any(|alias| alias == identifier))
            .cloned()
            .ok_or_else(|| ModelsError::NotFound(format!("Model '{identifier}' not found")))
    }

    async fn resolve_alias_cached(&self, identifier: &str) -> Option<String> {
        let models = self.cached_models().await.ok()?;
        models
            .iter()
            .find(|m| m.aliases.iter().any(|a| a == identifier))
            .map(|m| m.model_name.clone())
    }

    async fn get_configured_model_names(&self) -> Result<Vec<String>, ModelsError> {
        self.models_repository
            .get_configured_model_names()
            .await
            .map_err(|e| ModelsError::InternalError(e.to_string()))
    }

    async fn invalidate_models_cache(&self) {
        self.models_list_cache.invalidate_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference_provider_pool::BackendModelMetadata;
    use config::ExternalProvidersConfig;
    use uuid::Uuid;

    struct StaticModelsRepository {
        active_models: Vec<ModelWithPricing>,
        models_by_name: HashMap<String, ModelWithPricing>,
        resolved_models: HashMap<String, ModelWithPricing>,
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
            }
        }

        fn with_resolved_model(mut self, identifier: &str, model: ModelWithPricing) -> Self {
            self.resolved_models.insert(identifier.to_string(), model);
            self
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
    }

    fn test_catalog_model(model_name: &str) -> ModelWithPricing {
        test_catalog_model_with_output(model_name, Some(1024))
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
            openrouter_slug: None,
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

        let by_name = service.get_model_by_name(model_name).await.unwrap();
        let by_alias = service.get_model_by_name("friendly").await;
        let resolved_alias = service.resolve_and_get_model("friendly").await.unwrap();

        assert_eq!(by_name.context_length, 4096);
        assert_eq!(by_name.max_output_length, Some(1024));
        assert!(matches!(by_alias, Err(ModelsError::NotFound(_))));
        assert_eq!(resolved_alias.context_length, 4096);
        assert_eq!(resolved_alias.max_output_length, Some(1024));
    }
}
