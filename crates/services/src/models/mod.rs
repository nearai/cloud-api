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
/// sequential DB queries (count + list with JOIN+GROUP BY) on every hit.
/// With ~34 models in the system, pagination is pointless and the result
/// rarely changes — so we cache it in-process for a short window and
/// invalidate explicitly on admin writes (see `invalidate_models_cache`).
const MODELS_LIST_CACHE_TTL_SECS: u64 = 30;

/// Capacity for the model-list cache. We only ever store one entry
/// (keyed by `"all"`), so 1 is sufficient.
const MODELS_LIST_CACHE_CAPACITY: u64 = 1;

/// Cache key used for the single model-list entry.
const MODELS_LIST_CACHE_KEY: &str = "all";

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
        if let Some(cached) = self.models_list_cache.get(&MODELS_LIST_CACHE_KEY).await {
            return Ok((*cached).clone());
        }

        let models = self
            .models_repository
            .get_all_active_models()
            .await
            .map_err(|e| ModelsError::InternalError(e.to_string()))?;

        let arc = Arc::new(models);
        self.models_list_cache
            .insert(MODELS_LIST_CACHE_KEY, arc.clone())
            .await;
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
