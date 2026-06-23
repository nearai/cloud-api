pub mod ports;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moka::future::Cache;
pub use ports::{ModelInfo, ModelWithPricing, ModelsError, ModelsRepository, ModelsServiceTrait};

use crate::inference_provider_pool::InferenceProviderPool;

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

/// Capacity for the model-list cache. We only ever store one entry
/// (keyed by `"all"`), so 1 is sufficient.
const MODELS_LIST_CACHE_CAPACITY: u64 = 1;

/// Cache key used for the single model-list entry.
const MODELS_LIST_CACHE_KEY: &str = "all";

fn apply_backend_context_lengths(
    models: &mut [ModelWithPricing],
    context_lengths: &std::collections::HashMap<String, i32>,
) {
    for model in models {
        if let Some(context_length) = context_lengths.get(&model.model_name) {
            model.context_length = *context_length;
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
                let context_lengths = inference_provider_pool.max_context_lengths_by_model().await;
                apply_backend_context_lengths(&mut models, &context_lengths);
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
    use config::ExternalProvidersConfig;
    use uuid::Uuid;

    struct StaticModelsRepository {
        model: ModelWithPricing,
    }

    #[async_trait]
    impl ModelsRepository for StaticModelsRepository {
        async fn get_all_active_models(&self) -> Result<Vec<ModelWithPricing>, anyhow::Error> {
            Ok(vec![self.model.clone()])
        }

        async fn get_model_by_name(
            &self,
            model_name: &str,
        ) -> Result<Option<ModelWithPricing>, anyhow::Error> {
            Ok((model_name == self.model.model_name).then(|| self.model.clone()))
        }

        async fn resolve_and_get_model(
            &self,
            identifier: &str,
        ) -> Result<Option<ModelWithPricing>, anyhow::Error> {
            Ok((identifier == self.model.model_name).then(|| self.model.clone()))
        }

        async fn get_configured_model_names(&self) -> Result<Vec<String>, anyhow::Error> {
            Ok(vec![self.model.model_name.clone()])
        }
    }

    fn test_catalog_model(model_name: &str) -> ModelWithPricing {
        ModelWithPricing {
            id: Uuid::new_v4(),
            model_name: model_name.to_string(),
            model_display_name: model_name.to_string(),
            model_description: "test model".to_string(),
            model_icon: None,
            input_cost_per_token: 1,
            output_cost_per_token: 1,
            cost_per_image: 0,
            cache_read_cost_per_token: 0,
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
            max_output_length: Some(1024),
            supported_sampling_parameters: Vec::new(),
            supported_features: Vec::new(),
            datacenters: None,
            is_ready: None,
            deprecation_date: None,
            openrouter_slug: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn provider_model(model_name: &str, context_length: i32) -> inference_providers::ModelInfo {
        inference_providers::ModelInfo {
            id: model_name.to_string(),
            object: "model".to_string(),
            created: 0,
            owned_by: "test".to_string(),
            context_length: Some(context_length),
            max_model_len: None,
            top_provider: None,
        }
    }

    #[tokio::test]
    async fn get_models_with_pricing_caches_backend_context_lengths_until_invalidation() {
        let model_name = "test/model";
        let pool = Arc::new(InferenceProviderPool::new(
            None,
            ExternalProvidersConfig::default(),
        ));
        pool.register_provider(
            model_name.to_string(),
            Arc::new(inference_providers::mock::MockProvider::with_models(vec![
                provider_model(model_name, 32_768),
            ])),
        )
        .await;
        let service = ModelsServiceImpl::new(
            pool.clone(),
            Arc::new(StaticModelsRepository {
                model: test_catalog_model(model_name),
            }),
        );

        let first = service.get_models_with_pricing().await.unwrap();
        assert_eq!(first[0].context_length, 32_768);

        pool.register_provider(
            model_name.to_string(),
            Arc::new(inference_providers::mock::MockProvider::with_models(vec![
                provider_model(model_name, 65_536),
            ])),
        )
        .await;

        let cached = service.get_models_with_pricing().await.unwrap();
        assert_eq!(cached[0].context_length, 32_768);

        service.invalidate_models_cache().await;
        let refreshed = service.get_models_with_pricing().await.unwrap();
        assert_eq!(refreshed[0].context_length, 65_536);
    }
}
