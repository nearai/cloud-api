pub mod ports;

use std::sync::Arc;

use async_trait::async_trait;
use inference_providers::InferenceProvider;
pub use ports::{ModelInfo, ModelWithPricing, ModelsError, ModelsRepository, ModelsServiceTrait};

use crate::inference_provider_pool::InferenceProviderPool;

pub struct ModelsServiceImpl {
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub models_repository: Arc<dyn ModelsRepository>,
}

impl ModelsServiceImpl {
    pub fn new(
        inference_provider_pool: Arc<InferenceProviderPool>,
        models_repository: Arc<dyn ModelsRepository>,
    ) -> Self {
        Self {
            inference_provider_pool,
            models_repository,
        }
    }
}

#[async_trait]
impl ModelsServiceTrait for ModelsServiceImpl {
    async fn get_models(&self) -> Result<Vec<ModelInfo>, ModelsError> {
        self.inference_provider_pool
            .models()
            .await
            .map(|models| {
                models
                    .data
                    .into_iter()
                    .map(|model| ModelInfo {
                        created: model.created,
                        id: model.id,
                        object: model.object,
                        owned_by: model.owned_by,
                    })
                    .collect()
            })
            .map_err(|e| ModelsError::InternalError(e.to_string()))
    }

    async fn get_models_with_pricing(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ModelWithPricing>, i64), ModelsError> {
        let total = self
            .models_repository
            .get_all_active_models_count()
            .await
            .map_err(|e| ModelsError::InternalError(e.to_string()))?;

        let models = self
            .models_repository
            .get_all_active_models(limit, offset)
            .await
            .map_err(|e| ModelsError::InternalError(e.to_string()))?;
        Ok((models, total))
    }

    async fn get_model_by_name(&self, model_name: &str) -> Result<ModelWithPricing, ModelsError> {
        self.models_repository
            .get_model_by_name(model_name)
            .await
            .map_err(|e| ModelsError::InternalError(e.to_string()))?
            .ok_or_else(|| ModelsError::NotFound(format!("Model '{}' not found", model_name)))
    }

    async fn get_configured_model_names(&self) -> Result<Vec<String>, ModelsError> {
        self.models_repository
            .get_configured_model_names()
            .await
            .map_err(|e| ModelsError::InternalError(e.to_string()))
    }
}
