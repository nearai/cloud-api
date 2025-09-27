use async_trait::async_trait;
use inference_providers::{
    models::{CompletionError, ListModelsError, ModelsResponse, StreamChunk},
    AttestationReport, ChatCompletionParams, ChatSignature, CompletionParams, InferenceProvider,
    StreamingResult, StreamingResultExt,
};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct InferenceProviderPool {
    providers: Vec<Arc<dyn InferenceProvider + Send + Sync>>,
    model_mapping: Arc<RwLock<HashMap<String, Arc<dyn InferenceProvider + Send + Sync>>>>,
    chat_id_mapping: Arc<RwLock<HashMap<String, Arc<dyn InferenceProvider + Send + Sync>>>>,
}

impl InferenceProviderPool {
    pub fn new(providers: Vec<Arc<dyn InferenceProvider + Send + Sync>>) -> Self {
        Self {
            providers,
            model_mapping: Arc::new(RwLock::new(HashMap::new())),
            chat_id_mapping: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Initialize model discovery - should be called during application startup
    pub async fn initialize(&self) -> Result<(), ListModelsError> {
        tracing::info!(
            "Initializing model discovery for {} providers",
            self.providers.len()
        );
        match self.discover_models().await {
            Ok(models_response) => {
                tracing::info!(
                    "Successfully discovered {} models",
                    models_response.data.len()
                );
                Ok(())
            }
            Err(e) => {
                tracing::error!("Failed to initialize model discovery: {}", e);
                Err(e)
            }
        }
    }

    /// Ensure models are discovered before using them
    async fn ensure_models_discovered(&self) -> Result<(), CompletionError> {
        let model_mapping = self.model_mapping.read().await;

        // If mapping is empty, we need to discover models
        if model_mapping.is_empty() {
            drop(model_mapping); // Release read lock
            tracing::warn!("Model mapping is empty, triggering model discovery");
            self.discover_models().await.map_err(|e| {
                CompletionError::CompletionError(format!("Failed to discover models: {}", e))
            })?;
        }

        Ok(())
    }

    async fn discover_models(&self) -> Result<ModelsResponse, ListModelsError> {
        tracing::debug!(
            providers_count = self.providers.len(),
            "Starting model discovery across all providers"
        );

        // Collect all models from all providers
        let mut all_models = Vec::new();
        let mut model_mapping = self.model_mapping.write().await;
        model_mapping.clear();

        for (provider_idx, provider) in self.providers.iter().enumerate() {
            tracing::debug!(
                provider_index = provider_idx,
                "Discovering models from provider"
            );

            match provider.models().await {
                Ok(models_response) => {
                    tracing::info!(
                        provider_index = provider_idx,
                        models_count = models_response.data.len(),
                        "Successfully discovered models from provider"
                    );

                    for model in &models_response.data {
                        tracing::debug!(
                            provider_index = provider_idx,
                            model_id = %model.id,
                            "Adding model to mapping"
                        );
                        // Map each model to its provider
                        model_mapping.insert(model.id.clone(), provider.clone());
                        all_models.push(model.clone());
                    }
                }
                Err(e) => {
                    // Log error but continue with other providers
                    tracing::error!(
                        provider_index = provider_idx,
                        error = %e,
                        "Provider failed to list models, continuing with other providers"
                    );
                }
            }
        }

        tracing::info!(
            total_models = all_models.len(),
            total_providers = self.providers.len(),
            model_ids = ?all_models.iter().map(|m| &m.id).collect::<Vec<_>>(),
            "Model discovery completed"
        );

        Ok(ModelsResponse {
            object: "list".to_string(),
            data: all_models,
        })
    }

    /// Store a mapping of chat_id to provider
    async fn store_chat_id_mapping(
        &self,
        chat_id: String,
        provider: Arc<dyn InferenceProvider + Send + Sync>,
    ) {
        let mut mapping = self.chat_id_mapping.write().await;
        mapping.insert(chat_id.clone(), provider);
        tracing::debug!("Stored chat_id mapping: {}", chat_id);
    }

    /// Lookup provider by chat_id
    pub async fn get_provider_by_chat_id(
        &self,
        chat_id: &str,
    ) -> Option<Arc<dyn InferenceProvider + Send + Sync>> {
        let mapping = self.chat_id_mapping.read().await;
        mapping.get(chat_id).cloned()
    }
}

#[async_trait]
impl InferenceProvider for InferenceProviderPool {
    async fn get_signature(&self, chat_id: &str) -> Result<ChatSignature, CompletionError> {
        // First try to get the specific provider for this chat_id
        if let Some(provider) = self.get_provider_by_chat_id(chat_id).await {
            tracing::info!(
                chat_id = %chat_id,
                "Found mapped provider for chat_id, calling get_signature"
            );
            return provider.get_signature(chat_id).await;
        }

        // Fallback to trying all providers if chat_id mapping not found
        tracing::warn!(
            chat_id = %chat_id,
            "No provider mapping found for chat_id, trying all providers"
        );
        for provider in &self.providers {
            match provider.get_signature(chat_id).await {
                Ok(signature) => return Ok(signature),
                Err(_) => continue, // Try next provider
            }
        }
        Err(CompletionError::CompletionError(format!(
            "No provider found with signature for chat_id: {}",
            chat_id
        )))
    }

    async fn get_attestation_report(
        &self,
        signing_algo: Option<&str>,
    ) -> Result<AttestationReport, CompletionError> {
        // Delegate to the first provider that supports attestation reports
        for provider in &self.providers {
            match provider.get_attestation_report(signing_algo).await {
                Ok(report) => return Ok(report),
                Err(_) => continue, // Try next provider
            }
        }
        Err(CompletionError::CompletionError(
            "No provider found that supports attestation reports".to_string(),
        ))
    }

    async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        self.discover_models().await
    }

    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        let model_id = params.model.clone();

        tracing::debug!(
            model = %model_id,
            "Starting chat completion stream request"
        );

        // Ensure models are discovered first
        self.ensure_models_discovered().await?;

        let model_mapping = self.model_mapping.read().await;
        let available_models: Vec<_> = model_mapping.keys().collect();

        tracing::debug!(
            model_id = %model_id,
            available_models = ?available_models,
            mapping_size = model_mapping.len(),
            "Checking model availability in provider pool"
        );

        match model_mapping.get(&model_id) {
            Some(provider) => {
                tracing::info!(
                    model_id = %model_id,
                    "Found provider for model, calling chat_completion_stream"
                );
                let stream = provider.chat_completion_stream(params).await?;
                let mut peekable = StreamingResultExt::peekable(stream);
                if let Some(Ok(StreamChunk::Chat(chat_chunk))) = peekable.peek().await {
                    let chat_id = chat_chunk.id.clone();
                    let pool = self.clone();
                    let provider = provider.clone();
                    tokio::spawn(async move {
                        tracing::info!(
                            chat_id = %chat_id,
                            "Storing chat_id mapping"
                        );
                        pool.store_chat_id_mapping(chat_id, provider).await;
                    });
                }
                Ok(Box::pin(peekable))
            }
            None => {
                tracing::error!(
                    model_id = %model_id,
                    available_models = ?available_models,
                    providers_count = %self.providers.len(),
                    mapping_size = model_mapping.len(),
                    "Model not found in provider pool"
                );
                Err(CompletionError::CompletionError(format!(
                    "Model '{}' not found in any configured provider. Available models: {:?}",
                    model_id, available_models
                )))
            }
        }
    }

    async fn text_completion_stream(
        &self,
        params: CompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        let model_id = params.model.clone();

        // Ensure models are discovered first
        self.ensure_models_discovered().await?;

        let model_mapping = self.model_mapping.read().await;
        let available_models: Vec<_> = model_mapping.keys().collect();

        match model_mapping.get(&model_id) {
            Some(provider) => provider.text_completion_stream(params).await,
            None => {
                tracing::error!(
                    model_id = %model_id,
                    available_models = ?available_models,
                    providers_count = %self.providers.len(),
                    "Model not found in provider pool. Available models: {:?}",
                    available_models
                );
                Err(CompletionError::CompletionError(format!(
                    "Model '{}' not found in any configured provider. Available models: {:?}",
                    model_id, available_models
                )))
            }
        }
    }
}
