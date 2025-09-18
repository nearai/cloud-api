use async_trait::async_trait;
use dstack_sdk::dstack_client::DstackClient;
use futures::{Stream, stream, StreamExt};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

use crate::{
    errors::CompletionError,
    models::*,
    providers::{StreamChunk, vllm::VLlmProvider, ModelInfo},
    services::CompletionHandler,
};
use config::DomainConfig;

use crate::services::TdxHandler;

/// A completion service that routes requests to different providers based on model availability
pub struct ProviderRouter {
    providers: Vec<Arc<dyn CompletionHandler>>,
    model_mapping: Arc<RwLock<HashMap<String, Arc<dyn CompletionHandler>>>>,
    discovered_models: Arc<RwLock<Vec<ModelInfo>>>,
    pub config: DomainConfig,
}

impl ProviderRouter {
    pub fn new(config: DomainConfig) -> Self {
        Self {
            providers: Vec::new(),
            model_mapping: Arc::new(RwLock::new(HashMap::new())),
            discovered_models: Arc::new(RwLock::new(Vec::new())),
            config,
        }
    }
    
    /// Start periodic model discovery refresh in the background
    pub fn start_periodic_refresh(self: Arc<Self>) {
        let refresh_interval = self.config.model_discovery.refresh_interval;
        
        // Don't start refresh if in mock mode
        if self.config.use_mock {
            return;
        }
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(refresh_interval));
            loop {
                tracing::debug!("Starting periodic model discovery refresh...");
                
                match self.discover_models().await {
                    Ok(models) => {
                        tracing::info!("Periodic model refresh completed: {} models available", models.len());
                    }
                    Err(e) => {
                        tracing::warn!("Periodic model refresh failed: {}", e);
                    }
                }
                tracing::debug!("Waiting for next periodic model discovery refresh...");
                interval.tick().await;
            }
        });
    }
    
    /// Create a service from domain configuration
    pub fn from_domain_config(config: DomainConfig) -> Self {
        let mut service = Self {
            providers: Vec::new(),
            model_mapping: Arc::new(RwLock::new(HashMap::new())),
            discovered_models: Arc::new(RwLock::new(Vec::new())),
            config: config.clone(),
        };
        
        if config.use_mock {
            info!("Mock mode enabled, no real providers will be used");
            return service;
        }
        
        for provider_config in &config.providers {
            if !provider_config.enabled {
                info!("Skipping disabled provider: {}", provider_config.name);
                continue;
            }
            
            let provider = match provider_config.provider_type.as_str() {
                "vllm" => {
                    info!("Adding vLLM provider: {} at {}", provider_config.name, provider_config.url);
                    Arc::new(VLlmProvider::new(
                        provider_config.name.clone(),
                        provider_config.url.clone(),
                        provider_config.api_key.clone(),
                    )) as Arc<dyn CompletionHandler>
                }
                provider_type => {
                    error!("Unsupported provider type: {} for provider {}", provider_type, provider_config.name);
                    continue;
                }
            };
            
            service.providers.push(provider);
        }
        
        if service.providers.is_empty() && !config.use_mock {
            error!("No providers configured and mock mode disabled");
        }
        
        service
    }
    
    /// Discover models from all providers and update routing table
    pub async fn discover_models(&self) -> Result<Vec<ModelInfo>, CompletionError> {
        let mut all_models = Vec::new();
        let mut model_mapping = self.model_mapping.write().await;
        model_mapping.clear();
        
        // Create futures for all provider discoveries
        let provider_futures: Vec<_> = self.providers
            .iter()
            .map(|provider| {
                let provider_clone = provider.clone();
                async move {
                    let result = provider_clone.get_available_models().await;
                    (provider_clone, result)
                }
            })
            .collect();
        
        // Execute all futures in parallel
        let mut results = stream::iter(provider_futures)
            .buffer_unordered(10)  // Run up to 10 discoveries in parallel
            .collect::<Vec<_>>()
            .await;
        
        // Process the results
        for (provider, result) in results.drain(..) {
            match result {
                Ok(models) => {
                    info!("Discovered {} models from provider {}", models.len(), provider.name());
                    
                    // Update model mapping
                    for model in &models {
                        debug!("  - {} ({})", model.id, model.provider);
                        model_mapping.insert(model.id.clone(), provider.clone());
                    }
                    
                    all_models.extend(models);
                }
                Err(e) => {
                    error!("Failed to discover models from provider {}: {}", provider.name(), e);
                }
            }
        }
        
        info!("Model discovery complete: {} total models from {} providers", 
              all_models.len(), self.providers.len());
        
        // Store discovered models for later retrieval
        *self.discovered_models.write().await = all_models.clone();
              
        Ok(all_models)
    }
    
    
    /// Add a provider to the service
    pub async fn add_provider(&mut self, provider: Arc<dyn CompletionHandler>, models: Vec<String>) {
        let mut model_mapping = self.model_mapping.write().await;
        for model in models {
            model_mapping.insert(model, provider.clone());
        }
        self.providers.push(provider);
    }
    
    /// Get the provider for a specific model
    async fn get_provider(&self, model_id: &str) -> Result<Arc<dyn CompletionHandler>, CompletionError> {
        let model_mapping = self.model_mapping.read().await;
        model_mapping.get(model_id)
            .cloned()
            .ok_or_else(|| CompletionError::InvalidModel(format!("Model '{}' not available in any provider", model_id)))
    }
}

#[async_trait]
impl CompletionHandler for ProviderRouter {
    fn name(&self) -> &str {
        "provider-router"
    }
    
    fn supports_model(&self, model_id: &str) -> bool {
        // Note: This is now an async operation, but the trait requires sync
        // We'll use try_read() and default to false if the lock is not immediately available
        self.model_mapping.try_read()
            .map(|mapping| mapping.contains_key(model_id))
            .unwrap_or(false)
    }
    async fn chat_completion(&self, params: ChatCompletionParams) -> Result<ChatCompletionResult, CompletionError> {
        let provider = self.get_provider(&params.model_id).await?;
        debug!("Routing chat completion for model '{}' to provider '{}'", params.model_id, provider.name());
        provider.chat_completion(params).await
    }
    
    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        let provider = self.get_provider(&params.model_id).await?;
        debug!("Routing streaming chat completion for model '{}' to provider '{}'", params.model_id, provider.name());
        provider.chat_completion_stream(params).await
    }
    
    async fn text_completion(&self, params: CompletionParams) -> Result<CompletionResult, CompletionError> {
        let provider = self.get_provider(&params.model_id).await?;
        debug!("Routing text completion for model '{}' to provider '{}'", params.model_id, provider.name());
        provider.text_completion(params).await
    }
    
    async fn text_completion_stream(
        &self,
        params: CompletionParams,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        let provider = self.get_provider(&params.model_id).await?;
        debug!("Routing streaming text completion for model '{}' to provider '{}'", params.model_id, provider.name());
        provider.text_completion_stream(params).await
    }
    
    async fn get_available_models(&self) -> Result<Vec<crate::providers::ModelInfo>, CompletionError> {
        // Return the cached discovered models from the periodic check
        let models = self.discovered_models.read().await;
        Ok(models.clone())
    }
    
    
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl ProviderRouter {
    /// Load service from default configuration using dependency injection
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let api_config = config::ApiConfig::load()?;
        let domain_config = DomainConfig::from(api_config);
        Ok(Self::from_domain_config(domain_config))
    }
}


pub struct TdxHandlerImpl {
    client: DstackClient,
    config: DomainConfig,
}

impl TdxHandlerImpl {
    pub fn new(config: DomainConfig) -> Self {
        Self {
            client: DstackClient::new(Some(&config.dstack_client.url)),
            config,
        }
    }
}

#[async_trait]
impl TdxHandler for TdxHandlerImpl {
    
    async fn get_quote(&self) -> Result<QuoteResponse, CompletionError> {
        // Get system info (not currently used but available for future enhancements)
        let _info = self.client.info().await
            .map_err(|e| CompletionError::InternalError(format!("Failed to get system info: {}", e)))?;
        
        // Generate TDX quote with a standard identifier
        let quote_data = b"platform-api-gateway-quote";
        let quote_resp = self.client.get_quote(quote_data.to_vec()).await
            .map_err(|e| CompletionError::InternalError(format!("Failed to get TDX quote: {}", e)))?;
        
        // Parse RTMRs to get measurement
        let rtmrs = quote_resp.replay_rtmrs()
            .map_err(|e| CompletionError::InternalError(format!("Failed to replay RTMRs: {}", e)))?;
        
        // Extract MRENCLAVE from RTMRs (typically RTMR 0 contains the enclave measurement)
        let measurement = if let Some(rtmr) = rtmrs.get(&0) {
            // Convert the first RTMR to a formatted string - will be improved later
            format!("MRENCLAVE:{:?}", rtmr)
        } else {
            "MRENCLAVE:unknown".to_string()
        };
        
        // Build gateway quote response
        let gateway_quote = GatewayQuote {
            quote: quote_resp.quote,
            measurement,
            svn: 12, // TODO: Extract actual SVN from quote
            build: BuildInfo {
                image: "ghcr.io/agenthub/gateway:dev".to_string(), // TODO: Get from build metadata
                sbom: "sha256:placeholder".to_string(), // TODO: Get actual SBOM hash
            },
        };
        
        // Build allowlist from current provider configuration
        let mut allowlist = Vec::new();
        
        // Add configured vLLM providers to allowlist
        for provider in &self.config.providers {
            if provider.enabled {
                allowlist.push(ServiceAllowlistEntry {
                    service: format!("vllm-models/{}", provider.name),
                    expected_measurements: vec![
                        "sha256:placeholder-measurement".to_string() // TODO: Get real measurements
                    ],
                    min_svn: 10,
                    identifier: format!("ledger://compose/sha256:{}", provider.name.replace('-', "")),
                });
            }
        }
        
        Ok(QuoteResponse {
            gateway: gateway_quote,
            allowlist,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_service_creation() {
        let config = DomainConfig {
            use_mock: true,
            providers: vec![],
            model_discovery: config::ModelDiscoveryConfig {
                refresh_interval: 300,
                timeout: 30,
            },
            dstack_client: config::DstackClientConfig {
                url: "http://localhost:8000".to_string(),
            },
            auth: config::AuthConfig::default(),
        };
        let service = ProviderRouter::new(config);
        assert_eq!(service.providers.len(), 0);
        // Model mapping is now behind Arc<RwLock> so we can't directly check its size in tests
    }
    
    #[test]
    fn test_provider_service_from_domain_config() {
        use config::{ProviderConfig, ModelDiscoveryConfig};
        
        let config = DomainConfig {
            use_mock: false,
            providers: vec![
                ProviderConfig {
                    name: "test-vllm".to_string(),
                    provider_type: "vllm".to_string(),
                    url: "http://localhost:8000".to_string(),
                    api_key: Some("test-key".to_string()),
                    enabled: true,
                    priority: 1,
                },
            ],
            model_discovery: ModelDiscoveryConfig {
                refresh_interval: 300,
                timeout: 30,
            },
            dstack_client: config::DstackClientConfig {
                url: "http://localhost:8000".to_string(),
            },
            auth: config::AuthConfig::default(),
        };
        
        let service = ProviderRouter::from_domain_config(config);
        assert_eq!(service.providers.len(), 1);
        // Note: model_mapping is empty until discover_models() is called
    }
}
