use async_trait::async_trait;
use inference_providers::{
    models::{CompletionError, ListModelsError, ModelsResponse, StreamChunk},
    AttestationReport, ChatCompletionParams, ChatSignature, CompletionParams, InferenceProvider,
    StreamingResult, StreamingResultExt, VLlmConfig, VLlmProvider,
};
use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Duration};
use tokio::sync::RwLock;

type InferenceProviderTrait = dyn InferenceProvider + Send + Sync;

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
        discovery_timeout_secs: u64,
    ) -> Self {
        Self {
            discovery_url,
            api_key,
            discovery_timeout: Duration::from_secs(discovery_timeout_secs),
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
    async fn fetch_from_discovery(&self) -> Result<HashMap<String, String>, ListModelsError> {
        tracing::info!(
            url = %self.discovery_url,
            "Fetching models from discovery server"
        );

        let client = reqwest::Client::builder()
            .timeout(self.discovery_timeout)
            .build()
            .map_err(|e| {
                ListModelsError::FetchError(format!("Failed to create HTTP client: {}", e))
            })?;

        let response = client
            .get(&self.discovery_url)
            .send()
            .await
            .map_err(|e| ListModelsError::FetchError(format!("HTTP request failed: {}", e)))?;

        let discovery_map: HashMap<String, String> = response
            .json()
            .await
            .map_err(|e| ListModelsError::FetchError(format!("Failed to parse JSON: {}", e)))?;

        tracing::debug!(entries = discovery_map.len(), "Received discovery response");

        Ok(discovery_map)
    }

    /// Parse IP-based keys like "REDACTED_IP2:8000"
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
                CompletionError::CompletionError(format!("Failed to discover models: {}", e))
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

        for (key, model_name) in discovery_map {
            // Filter out non-IP keys
            if let Some((ip, port)) = Self::parse_ip_port(&key) {
                tracing::debug!(
                    key = %key,
                    model = %model_name,
                    ip = %ip,
                    port = port,
                    "Adding IP-based provider"
                );

                model_to_endpoints
                    .entry(model_name)
                    .or_default()
                    .push((ip, port));
            } else {
                tracing::debug!(
                    key = %key,
                    model = %model_name,
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
                let url = format!("http://{}:{}", ip, port);
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

    /// Get the next provider for a model using round-robin load balancing
    async fn get_next_provider_for_model(
        &self,
        model_id: &str,
    ) -> Option<Arc<dyn InferenceProvider + Send + Sync>> {
        let model_mapping = self.model_mapping.read().await;
        let providers = model_mapping.get(model_id)?;

        if providers.is_empty() {
            return None;
        }

        if providers.len() == 1 {
            return Some(providers[0].clone());
        }

        // Get current index for this model
        let mut indices = self.load_balancer_index.write().await;
        let index = indices.entry(model_id.to_string()).or_insert(0);
        let selected_index = *index % providers.len();
        let provider = providers[selected_index].clone();

        // Increment for next request
        *index = (*index + 1) % providers.len();

        tracing::debug!(
            model = %model_id,
            providers_count = providers.len(),
            selected_index = selected_index,
            "Selected provider using round-robin load balancing"
        );

        Some(provider)
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

        // Fallback to trying all discovered providers if chat_id mapping not found
        tracing::warn!(
            chat_id = %chat_id,
            "No provider mapping found for chat_id, trying all discovered providers"
        );

        let model_mapping = self.model_mapping.read().await;
        for providers in model_mapping.values() {
            for provider in providers {
                match provider.get_signature(chat_id).await {
                    Ok(signature) => return Ok(signature),
                    Err(_) => continue, // Try next provider
                }
            }
        }

        Err(CompletionError::CompletionError(format!(
            "No provider found with signature for chat_id: {}",
            chat_id
        )))
    }

    async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
    ) -> Result<AttestationReport, CompletionError> {
        // Get the first provider for this model
        if let Some(provider) = self.get_next_provider_for_model(&model).await {
            return provider.get_attestation_report(model, signing_algo).await;
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

        // Use load balancing to get the next provider
        match self.get_next_provider_for_model(&model_id).await {
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
                let model_mapping = self.model_mapping.read().await;
                let available_models: Vec<_> = model_mapping.keys().collect();

                tracing::error!(
                    model_id = %model_id,
                    available_models = ?available_models,
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

        // Use load balancing to get the next provider
        match self.get_next_provider_for_model(&model_id).await {
            Some(provider) => provider.text_completion_stream(params).await,
            None => {
                let model_mapping = self.model_mapping.read().await;
                let available_models: Vec<_> = model_mapping.keys().collect();

                tracing::error!(
                    model_id = %model_id,
                    available_models = ?available_models,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ip_port() {
        // Valid IP-based keys
        assert_eq!(
            InferenceProviderPool::parse_ip_port("REDACTED_IP2:8000"),
            Some(("REDACTED_IP2".to_string(), 8000))
        );
        assert_eq!(
            InferenceProviderPool::parse_ip_port("REDACTED_IP:8001"),
            Some(("REDACTED_IP".to_string(), 8001))
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
