use inference_providers::{
    models::{CompletionError, ListModelsError, ModelsResponse},
    ChatCompletionParams, InferenceProvider, StreamingResult, StreamingResultExt, VLlmConfig,
    VLlmProvider,
};
use serde::Deserialize;
use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Duration};
use tokio::sync::RwLock;

type InferenceProviderTrait = dyn InferenceProvider + Send + Sync;

/// Discovery entry returned by the discovery layer
#[derive(Debug, Clone, Deserialize)]
struct DiscoveryEntry {
    /// Model identifier (e.g., "deepseek-ai/DeepSeek-V3.1")
    model: String,
    /// Tags for filtering/routing (e.g., ["prod", "dev"])
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Clone)]
pub struct InferenceProviderPool {
    /// Discovery URL for dynamic model discovery
    discovery_url: String,
    /// Optional API key for authenticating with discovered providers
    api_key: Option<String>,
    /// HTTP timeout for discovery requests
    discovery_timeout: Duration,
    /// Map of model name -> list of providers (for load balancing)
    model_mapping: Arc<RwLock<HashMap<String, Vec<Arc<InferenceProviderTrait>>>>>,
    /// Round-robin index for each model
    load_balancer_index: Arc<RwLock<HashMap<String, usize>>>,
    /// Map of chat_id -> provider for sticky routing
    chat_id_mapping: Arc<RwLock<HashMap<String, Arc<InferenceProviderTrait>>>>,
}

impl InferenceProviderPool {
    /// Create a new pool with discovery URL and optional API key
    pub fn new(
        discovery_url: String,
        api_key: Option<String>,
        discovery_timeout_secs: i64,
    ) -> Self {
        Self {
            discovery_url,
            api_key,
            discovery_timeout: Duration::from_secs(discovery_timeout_secs as u64),
            model_mapping: Arc::new(RwLock::new(HashMap::new())),
            load_balancer_index: Arc::new(RwLock::new(HashMap::new())),
            chat_id_mapping: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Initialize model discovery - should be called during application startup
    pub async fn initialize(&self) -> Result<(), ListModelsError> {
        tracing::info!(
            url = %self.discovery_url,
            "Initializing model discovery from discovery server"
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

    /// Fetch and parse models from discovery endpoint
    async fn fetch_from_discovery(
        &self,
    ) -> Result<HashMap<String, DiscoveryEntry>, ListModelsError> {
        tracing::info!(
            url = %self.discovery_url,
            "Fetching models from discovery server"
        );

        let client = reqwest::Client::builder()
            .timeout(self.discovery_timeout)
            .build()
            .map_err(|e| {
                ListModelsError::FetchError(format!("Failed to create HTTP client: {e}"))
            })?;

        let response = client
            .get(&self.discovery_url)
            .send()
            .await
            .map_err(|e| ListModelsError::FetchError(format!("HTTP request failed: {e}")))?;

        let discovery_map: HashMap<String, DiscoveryEntry> = response
            .json()
            .await
            .map_err(|e| ListModelsError::FetchError(format!("Failed to parse JSON: {e}")))?;

        tracing::debug!(entries = discovery_map.len(), "Received discovery response");

        Ok(discovery_map)
    }

    /// Parse IP-based keys like "160.72.54.186:8000"
    /// Returns None for keys that don't match IP:PORT format (e.g., "redpill:...")
    fn parse_ip_port(key: &str) -> Option<(String, u16)> {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() != 2 {
            return None;
        }

        let ip = parts[0];
        let port = parts[1].parse::<u16>().ok()?;

        // Verify it's a valid IP address
        if ip.parse::<IpAddr>().is_err() {
            return None;
        }

        Some((ip.to_string(), port))
    }

    /// Ensure models are discovered before using them
    async fn ensure_models_discovered(&self) -> Result<(), CompletionError> {
        let model_mapping = self.model_mapping.read().await;

        // If mapping is empty, we need to discover models
        if model_mapping.is_empty() {
            drop(model_mapping); // Release read lock
            tracing::warn!("Model mapping is empty, triggering model discovery");
            self.discover_models().await.map_err(|e| {
                CompletionError::CompletionError(format!("Failed to discover models: {e}"))
            })?;
        }

        Ok(())
    }

    async fn discover_models(&self) -> Result<ModelsResponse, ListModelsError> {
        tracing::info!("Starting model discovery from discovery endpoint");

        // Fetch from discovery server
        let discovery_map = self.fetch_from_discovery().await?;

        let mut model_mapping = self.model_mapping.write().await;
        model_mapping.clear();

        // Group by model name
        let mut model_to_endpoints: HashMap<String, Vec<(String, u16)>> = HashMap::new();

        for (key, entry) in discovery_map {
            // Filter out non-IP keys
            if let Some((ip, port)) = Self::parse_ip_port(&key) {
                tracing::debug!(
                    key = %key,
                    model = %entry.model,
                    tags = ?entry.tags,
                    ip = %ip,
                    port = port,
                    "Adding IP-based provider"
                );

                model_to_endpoints
                    .entry(entry.model)
                    .or_default()
                    .push((ip, port));
            } else {
                tracing::debug!(
                    key = %key,
                    model = %entry.model,
                    tags = ?entry.tags,
                    "Skipping non-IP key"
                );
            }
        }

        // Create providers for each endpoint
        let mut all_models = Vec::new();

        for (model_name, endpoints) in model_to_endpoints {
            tracing::info!(
                model = %model_name,
                providers_count = endpoints.len(),
                "Discovered model with {} provider(s)",
                endpoints.len()
            );

            let mut providers_for_model = Vec::new();

            for (ip, port) in endpoints {
                let url = format!("http://{ip}:{port}");
                tracing::debug!(
                    model = %model_name,
                    url = %url,
                    "Creating provider for model"
                );

                let provider = Arc::new(VLlmProvider::new(VLlmConfig::new(
                    url,
                    self.api_key.clone(),
                    None,
                ))) as Arc<InferenceProviderTrait>;

                providers_for_model.push(provider);
            }

            model_mapping.insert(model_name.clone(), providers_for_model);

            all_models.push(inference_providers::models::ModelInfo {
                id: model_name,
                object: "model".to_string(),
                created: 0,
                owned_by: "discovered".to_string(),
            });
        }

        tracing::info!(
            total_models = all_models.len(),
            total_providers = model_mapping.values().map(|v| v.len()).sum::<usize>(),
            model_ids = ?all_models.iter().map(|m| &m.id).collect::<Vec<_>>(),
            "Model discovery from endpoint completed"
        );

        Ok(ModelsResponse {
            object: "list".to_string(),
            data: all_models,
        })
    }

    async fn get_providers_for_model(
        &self,
        model_id: &str,
    ) -> Option<Vec<Arc<InferenceProviderTrait>>> {
        let model_mapping = self.model_mapping.read().await;
        model_mapping.get(model_id).cloned()
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

    /// Get providers for a model in priority order for fallback
    /// Returns providers with the round-robin selected one first, followed by others
    async fn get_providers_with_fallback(
        &self,
        model_id: &str,
    ) -> Option<Vec<Arc<InferenceProviderTrait>>> {
        let model_mapping = self.model_mapping.read().await;
        let providers = model_mapping.get(model_id)?;

        if providers.is_empty() {
            return None;
        }

        if providers.len() == 1 {
            return Some(vec![providers[0].clone()]);
        }

        // Get current index for round-robin
        let mut indices = self.load_balancer_index.write().await;
        let index = indices.entry(model_id.to_string()).or_insert(0);
        let selected_index = *index % providers.len();

        // Increment for next request
        *index = (*index + 1) % providers.len();

        // Build ordered list following round-robin pattern:
        // selected provider first, then continue round-robin (selected+1, selected+2, ...)
        let mut ordered_providers = Vec::with_capacity(providers.len());
        for i in 0..providers.len() {
            let provider_index = (selected_index + i) % providers.len();
            ordered_providers.push(providers[provider_index].clone());
        }

        tracing::debug!(
            model = %model_id,
            providers_count = providers.len(),
            selected_index = selected_index,
            "Prepared providers for fallback with round-robin priority"
        );

        Some(ordered_providers)
    }

    /// Generic retry helper that tries each provider in order with automatic fallback
    /// Returns both the result and the provider that succeeded (for chat_id mapping)
    async fn retry_with_fallback<T, F, Fut>(
        &self,
        model_id: &str,
        operation_name: &str,
        provider_fn: F,
    ) -> Result<(T, Arc<InferenceProviderTrait>), CompletionError>
    where
        F: Fn(Arc<InferenceProviderTrait>) -> Fut,
        Fut: std::future::Future<Output = Result<T, CompletionError>>,
    {
        // Ensure models are discovered first
        self.ensure_models_discovered().await?;

        // Get all providers with fallback priority
        let providers = match self.get_providers_with_fallback(model_id).await {
            Some(p) => p,
            None => {
                let model_mapping = self.model_mapping.read().await;
                let available_models: Vec<_> = model_mapping.keys().collect();

                tracing::error!(
                    model_id = %model_id,
                    available_models = ?available_models,
                    operation = operation_name,
                    "Model not found in provider pool"
                );
                return Err(CompletionError::CompletionError(format!(
                    "Model '{model_id}' not found in any configured provider. Available models: {available_models:?}"
                )));
            }
        };

        tracing::info!(
            model_id = %model_id,
            providers_count = providers.len(),
            operation = operation_name,
            "Attempting {} with {} provider(s)",
            operation_name,
            providers.len()
        );

        // Collect errors from all providers to surface to user
        let mut provider_errors = Vec::new();

        // Try each provider in order until one succeeds
        for (attempt, provider) in providers.iter().enumerate() {
            tracing::debug!(
                model_id = %model_id,
                attempt = attempt + 1,
                total_providers = providers.len(),
                operation = operation_name,
                "Trying provider {} of {}",
                attempt + 1,
                providers.len()
            );

            match provider_fn(provider.clone()).await {
                Ok(result) => {
                    tracing::info!(
                        model_id = %model_id,
                        attempt = attempt + 1,
                        operation = operation_name,
                        "Successfully completed request with provider"
                    );
                    return Ok((result, provider.clone()));
                }
                Err(e) => {
                    tracing::warn!(
                        model_id = %model_id,
                        attempt = attempt + 1,
                        error = %e,
                        operation = operation_name,
                        "Provider failed, will try next provider if available"
                    );
                    provider_errors.push(format!("Provider {}: {}", attempt + 1, e));
                }
            }
        }

        // All providers failed - include error details
        let error_details = provider_errors.join("; ");
        tracing::error!(
            model_id = %model_id,
            providers_tried = providers.len(),
            operation = operation_name,
            errors = %error_details,
            "All providers failed for model"
        );

        Err(CompletionError::CompletionError(format!(
            "All {} provider(s) failed for model '{}' during {}: {}",
            providers.len(),
            model_id,
            operation_name,
            error_details
        )))
    }

    pub async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, CompletionError> {
        // Get all providers for this model
        let mut all_attestations = vec![];

        if let Some(providers) = self.get_providers_for_model(&model).await {
            // Broadcast to all providers
            for provider in providers {
                match provider
                    .get_attestation_report(
                        model.clone(),
                        signing_algo.clone(),
                        nonce.clone(),
                        signing_address.clone(),
                    )
                    .await
                {
                    Ok(mut attestation) => {
                        // Remove 'all_attestations' field if present
                        attestation.remove("all_attestations");
                        all_attestations.push(attestation);
                    }
                    Err(e) => {
                        // Log and continue to next provider (404 is expected when
                        // signing_address doesn't match)
                        tracing::debug!(
                            model = %model,
                            error = %e,
                            "Provider returned error for attestation request, continuing to next provider"
                        );
                    }
                }
            }
        }

        if all_attestations.is_empty() {
            return Err(CompletionError::CompletionError(format!(
                "No provider found that supports attestation reports for model: {model}"
            )));
        }

        Ok(all_attestations)
    }

    pub async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        self.discover_models().await
    }

    pub async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        let model_id = params.model.clone();

        tracing::debug!(
            model = %model_id,
            "Starting chat completion stream request"
        );

        let (stream, provider) = self
            .retry_with_fallback(&model_id, "chat_completion_stream", |provider| {
                let params = params.clone();
                let request_hash = request_hash.clone();
                async move { provider.chat_completion_stream(params, request_hash).await }
            })
            .await?;

        // Store chat_id mapping for sticky routing by peeking at the first event
        let mut peekable = StreamingResultExt::peekable(stream);
        if let Some(Ok(event)) = peekable.peek().await {
            if let inference_providers::StreamChunk::Chat(chat_chunk) = &event.chunk {
                let chat_id = chat_chunk.id.clone();
                let pool = self.clone();
                tokio::spawn(async move {
                    tracing::info!(
                        chat_id = %chat_id,
                        "Storing chat_id mapping"
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

        let (response, provider) = self
            .retry_with_fallback(&model_id, "chat_completion", |provider| {
                let params = params.clone();
                let request_hash = request_hash.clone();
                async move { provider.chat_completion(params, request_hash).await }
            })
            .await?;

        // Store the chat_id mapping SYNCHRONOUSLY before returning
        // This ensures the attestation service can find the provider
        let chat_id = response.response.id.clone();
        tracing::info!(
            chat_id = %chat_id,
            "Storing chat_id mapping for non-streaming completion"
        );
        self.store_chat_id_mapping(chat_id.clone(), provider).await;
        tracing::debug!(
            chat_id = %chat_id,
            "Stored chat_id mapping before returning response"
        );

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ip_port() {
        // Valid IP-based keys
        assert_eq!(
            InferenceProviderPool::parse_ip_port("160.72.54.186:8000"),
            Some(("160.72.54.186".to_string(), 8000))
        );
        assert_eq!(
            InferenceProviderPool::parse_ip_port("154.57.34.78:8001"),
            Some(("154.57.34.78".to_string(), 8001))
        );

        // Invalid keys (should be filtered out)
        assert_eq!(
            InferenceProviderPool::parse_ip_port("redpill:phala/qwen-2.5-7b-instruct"),
            None
        );
        assert_eq!(InferenceProviderPool::parse_ip_port("invalid"), None);
        assert_eq!(InferenceProviderPool::parse_ip_port("not-an-ip:8000"), None);
        assert_eq!(
            InferenceProviderPool::parse_ip_port("256.256.256.256:8000"),
            None
        ); // Invalid IP
        assert_eq!(
            InferenceProviderPool::parse_ip_port("192.168.1.1:70000"),
            None
        ); // Invalid port
    }
}
