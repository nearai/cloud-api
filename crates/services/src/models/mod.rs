pub mod ports;

use std::sync::Arc;

use async_trait::async_trait;
use inference_providers::InferenceProvider;
pub use ports::{ModelInfo, ModelsError, ModelsService};

use crate::inference_provider_pool::InferenceProviderPool;

pub struct ModelsServiceImpl {
    pub inference_provider_pool: Arc<InferenceProviderPool>,
}

impl ModelsServiceImpl {
    pub fn new(inference_provider_pool: Arc<InferenceProviderPool>) -> Self {
        Self {
            inference_provider_pool,
        }
    }
}

#[async_trait]
impl ModelsService for ModelsServiceImpl {
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
}
