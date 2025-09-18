use std::{collections::HashMap, sync::Arc};
use inference_providers::{models::{CompletionError, ListModelsError, ModelsResponse}, ChatCompletionParams, CompletionParams, InferenceProvider, StreamingResult};
use tokio::sync::RwLock;
use async_trait::async_trait;

pub struct InferenceProviderPool {
    providers: Vec<Arc<dyn InferenceProvider + Send + Sync>>,
    model_mapping: Arc<RwLock<HashMap<String, Arc<dyn InferenceProvider + Send + Sync>>>>,
}

impl InferenceProviderPool {
    pub fn new(providers: Vec<Arc<dyn InferenceProvider + Send + Sync>>) -> Self {
        Self { providers, model_mapping: Arc::new(RwLock::new(HashMap::new())) }
    }

    async fn discover_models(&self) -> Result<ModelsResponse, ListModelsError> {
        // Collect all models from all providers
        let mut all_models = Vec::new();
        let mut model_mapping = self.model_mapping.write().await;
        model_mapping.clear();
        
        for provider in &self.providers {
            match provider.models().await {
                Ok(models_response) => {
                    for model in &models_response.data {
                        // Map each model to its provider
                        model_mapping.insert(model.id.clone(), provider.clone());
                        all_models.push(model.clone());
                    }
                }
                Err(e) => {
                    // Log error but continue with other providers
                    eprintln!("Warning: Provider failed to list models: {}", e);
                }
            }
        }
        
        Ok(ModelsResponse {
            object: "list".to_string(),
            data: all_models,
        })
    }
}

#[async_trait]
impl InferenceProvider for InferenceProviderPool {
    async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        self.discover_models().await
    }
    
    async fn chat_completion_stream(&self, params: ChatCompletionParams) -> Result<StreamingResult, CompletionError> {
        let model_id = &params.model;
        let model_mapping = self.model_mapping.read().await;
        
        match model_mapping.get(model_id) {
            Some(provider) => {
                provider.chat_completion_stream(params).await
            }
            None => {
                Err(CompletionError::CompletionError(format!(
                    "Model '{}' not found in any configured provider", model_id
                )))
            }
        }
    }
    
    async fn text_completion_stream(&self, params: CompletionParams) -> Result<StreamingResult, CompletionError> {
        let model_id = &params.model;
        let model_mapping = self.model_mapping.read().await;
        
        match model_mapping.get(model_id) {
            Some(provider) => {
                provider.text_completion_stream(params).await
            }
            None => {
                Err(CompletionError::CompletionError(format!(
                    "Model '{}' not found in any configured provider", model_id
                )))
            }
        }
    }
}

