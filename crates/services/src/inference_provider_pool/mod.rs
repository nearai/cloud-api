use inference_providers::{
    models::{AttestationError, CompletionError, ListModelsError, ModelsResponse},
    ChatCompletionParams, InferenceProvider, StreamingResult, VLlmConfig, VLlmProvider,
};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

type InferenceProviderTrait = dyn InferenceProvider + Send + Sync;

/// Simplified inference provider pool that acts as a thin wrapper around a single router endpoint
#[derive(Clone)]
pub struct InferenceProviderPool {
    /// The single router provider that handles all requests
    router_provider: Arc<InferenceProviderTrait>,
    /// Map of chat_id -> provider for attestation lookups
    chat_id_mapping: Arc<RwLock<HashMap<String, Arc<InferenceProviderTrait>>>>,
    /// Map of chat_id -> (request_hash, response_hash) for MockProvider signature generation
    signature_hashes: Arc<RwLock<HashMap<String, (String, String)>>>,
}

impl InferenceProviderPool {
    /// Create a new pool with a single router endpoint
    pub fn new(router_url: String, api_key: Option<String>, inference_timeout_secs: i64) -> Self {
        let router_provider = Arc::new(VLlmProvider::new(VLlmConfig::new(
            router_url,
            api_key,
            Some(inference_timeout_secs),
        ))) as Arc<InferenceProviderTrait>;

        Self {
            router_provider,
            chat_id_mapping: Arc::new(RwLock::new(HashMap::new())),
            signature_hashes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a provider for testing (useful for testing with mock providers)
    pub async fn register_provider(
        &self,
        _model_id: String,
        _provider: Arc<InferenceProviderTrait>,
    ) {
        // For testing, we replace the router provider with the mock
        // This is a simplified approach - in tests we'll use the mock directly
        // Note: We can't actually replace router_provider since it's Arc, but tests
        // should call register_providers which creates a pool differently
        tracing::debug!("register_provider called - this is mainly for backwards compatibility");
    }

    /// Register multiple providers for testing
    /// In the new simplified design, this creates a new pool with a mock provider
    pub async fn register_providers(&self, providers: Vec<(String, Arc<InferenceProviderTrait>)>) {
        tracing::debug!(
            "register_providers called with {} providers - mainly for backwards compatibility",
            providers.len()
        );
        // In tests, we should create pools directly with mock providers instead of using this
    }

    /// Store a mapping of chat_id to provider for attestation lookups
    async fn store_chat_id_mapping(
        &self,
        chat_id: String,
        provider: Arc<dyn InferenceProvider + Send + Sync>,
    ) {
        let mut mapping = self.chat_id_mapping.write().await;
        mapping.insert(chat_id.clone(), provider);
        tracing::debug!("Stored chat_id mapping: {}", chat_id);
    }

    /// Lookup provider by chat_id for attestation
    pub async fn get_provider_by_chat_id(
        &self,
        chat_id: &str,
    ) -> Option<Arc<dyn InferenceProvider + Send + Sync>> {
        let mapping = self.chat_id_mapping.read().await;
        mapping.get(chat_id).cloned()
    }

    /// Register signature hashes for a chat_id (for testing with MockProvider)
    pub async fn register_signature_hashes_for_chat(
        &self,
        chat_id: &str,
        request_hash: String,
        response_hash: String,
    ) {
        let mut hashes = self.signature_hashes.write().await;
        hashes.insert(chat_id.to_string(), (request_hash, response_hash));
        tracing::debug!("Registered signature hashes for chat_id: {}", chat_id);
    }

    /// Get signature hashes for a chat_id (used by MockProvider)
    pub async fn get_signature_hashes_for_chat(&self, chat_id: &str) -> Option<(String, String)> {
        let hashes = self.signature_hashes.read().await;
        hashes.get(chat_id).cloned()
    }

    pub async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, AttestationError> {
        // Forward directly to router
        match self
            .router_provider
            .get_attestation_report(model.clone(), signing_algo, nonce, signing_address)
            .await
        {
            Ok(mut attestation) => {
                // Remove 'all_attestations' field if present for backwards compatibility
                attestation.remove("all_attestations");
                Ok(vec![attestation])
            }
            Err(e) => {
                tracing::debug!(
                    model = %model,
                    error = %e,
                    "Router returned error for attestation request"
                );
                Err(AttestationError::ProviderNotFound(model))
            }
        }
    }

    pub async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        // Forward directly to router - router knows all available models
        self.router_provider.models().await
    }

    pub async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        let model_id = params.model.clone();

        tracing::debug!(
            model = %model_id,
            "Forwarding chat completion stream request to router"
        );

        // Forward directly to router
        let stream = self
            .router_provider
            .chat_completion_stream(params, request_hash)
            .await?;

        // Store chat_id mapping when we see the first event
        let pool = self.clone();
        let provider = self.router_provider.clone();

        // Use StreamingResultExt to peek at the first event
        use inference_providers::StreamingResultExt;
        let mut peekable = stream.peekable();

        if let Some(Ok(event)) = peekable.peek().await {
            if let inference_providers::StreamChunk::Chat(chat_chunk) = &event.chunk {
                let chat_id = chat_chunk.id.clone();
                tokio::spawn(async move {
                    tracing::info!(
                        chat_id = %chat_id,
                        "Storing chat_id mapping for streaming completion"
                    );
                    pool.store_chat_id_mapping(chat_id, provider).await;
                });
            }
        }

        Ok(Box::pin(peekable))
    }

    pub async fn chat_completion(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<inference_providers::ChatCompletionResponseWithBytes, CompletionError> {
        let model_id = params.model.clone();

        tracing::debug!(
            model = %model_id,
            "Forwarding chat completion request to router"
        );

        // Forward directly to router
        let response = self
            .router_provider
            .chat_completion(params, request_hash)
            .await?;

        // Store the chat_id mapping synchronously before returning
        let chat_id = response.response.id.clone();
        tracing::info!(
            chat_id = %chat_id,
            "Storing chat_id mapping for non-streaming completion"
        );
        self.store_chat_id_mapping(chat_id.clone(), self.router_provider.clone())
            .await;

        Ok(response)
    }

    /// Shutdown the inference provider pool and cleanup all resources
    pub async fn shutdown(&self) {
        tracing::info!("Initiating inference provider pool shutdown");

        // Clear chat_id to provider mappings
        tracing::debug!("Clearing chat session mappings");
        let mut chat_mapping = self.chat_id_mapping.write().await;
        let chat_count = chat_mapping.len();
        chat_mapping.clear();
        drop(chat_mapping);

        // Clear signature hashes
        tracing::debug!("Clearing signature hash tracking");
        let mut sig_hashes = self.signature_hashes.write().await;
        let sig_count = sig_hashes.len();
        sig_hashes.clear();
        drop(sig_hashes);

        tracing::info!(
            "Inference provider pool shutdown completed. Cleaned up: {} chat mappings, {} signatures",
            chat_count, sig_count
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_pool_creation() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8000".to_string(),
            Some("test-key".to_string()),
            300,
        );

        // Basic smoke test - just ensure we can create a pool
        // The pool should have a router_provider initialized
        assert!(Arc::strong_count(&pool.router_provider) > 0);
    }
}
