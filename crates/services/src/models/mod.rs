pub mod ports;

use std::sync::Arc;

use async_trait::async_trait;
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
}
