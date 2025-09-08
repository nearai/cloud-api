use async_trait::async_trait;
use futures::Stream;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{debug, error, info};

use crate::{
    errors::CompletionError,
    models::*,
    providers::{CompletionProvider, StreamChunk, vllm::VLlmProvider, ModelInfo},
    services::CompletionService,
};
use config::DomainConfig;

// ============================================================================
// Provider-based Completion Service
// ============================================================================

/// A completion service that routes requests to different providers based on model availability
pub struct ProviderService {
    providers: Vec<Arc<dyn CompletionProvider>>,
    model_mapping: HashMap<String, Arc<dyn CompletionProvider>>,
    pub config: DomainConfig,
}

impl ProviderService {
    pub fn new(config: DomainConfig) -> Self {
        Self {
            providers: Vec::new(),
            model_mapping: HashMap::new(),
            config,
        }
    }
    
    /// Create a service from domain configuration
    pub fn from_domain_config(config: DomainConfig) -> Self {
        let mut service = Self {
            providers: Vec::new(),
            model_mapping: HashMap::new(),
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
                    )) as Arc<dyn CompletionProvider>
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
    pub async fn discover_models(&mut self) -> Result<Vec<ModelInfo>, CompletionError> {
        let mut all_models = Vec::new();
        self.model_mapping.clear();
        
        for provider in &self.providers {
            match provider.get_models().await {
                Ok(models) => {
                    info!("Discovered {} models from provider {}", models.len(), provider.name());
                    
                    // Update model mapping
                    for model in &models {
                        debug!("  - {} ({})", model.id, model.provider);
                        self.model_mapping.insert(model.id.clone(), provider.clone());
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
              
        Ok(all_models)
    }
    
    /// Get all available models
    pub async fn get_available_models(&self) -> Result<Vec<ModelInfo>, CompletionError> {
        let mut all_models = Vec::new();
        
        for provider in &self.providers {
            match provider.get_models().await {
                Ok(models) => all_models.extend(models),
                Err(e) => error!("Failed to get models from {}: {}", provider.name(), e),
            }
        }
        
        Ok(all_models)
    }
    
    /// Add a provider to the service
    pub fn add_provider(&mut self, provider: Arc<dyn CompletionProvider>, models: Vec<String>) {
        for model in models {
            self.model_mapping.insert(model, provider.clone());
        }
        self.providers.push(provider);
    }
    
    /// Get the provider for a specific model
    fn get_provider(&self, model_id: &str) -> Result<&Arc<dyn CompletionProvider>, CompletionError> {
        self.model_mapping.get(model_id)
            .ok_or_else(|| CompletionError::InvalidModel(format!("Model '{}' not available in any provider", model_id)))
    }
}

#[async_trait]
impl CompletionService for ProviderService {
    async fn chat_completion(&self, params: ChatCompletionParams) -> Result<ChatCompletionResult, CompletionError> {
        let provider = self.get_provider(&params.model_id)?;
        debug!("Routing chat completion for model '{}' to provider '{}'", params.model_id, provider.name());
        provider.chat_completion(params).await
    }
    
    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        let provider = self.get_provider(&params.model_id)?;
        debug!("Routing streaming chat completion for model '{}' to provider '{}'", params.model_id, provider.name());
        provider.chat_completion_stream(params).await
    }
    
    async fn text_completion(&self, params: CompletionParams) -> Result<CompletionResult, CompletionError> {
        let provider = self.get_provider(&params.model_id)?;
        debug!("Routing text completion for model '{}' to provider '{}'", params.model_id, provider.name());
        provider.text_completion(params).await
    }
    
    async fn text_completion_stream(
        &self,
        params: CompletionParams,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        let provider = self.get_provider(&params.model_id)?;
        debug!("Routing streaming text completion for model '{}' to provider '{}'", params.model_id, provider.name());
        provider.text_completion_stream(params).await
    }
    
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ============================================================================
// Configuration Helpers
// ============================================================================

impl ProviderService {
    /// Load service from configuration file using dependency injection 
    pub fn from_config_file<P: AsRef<std::path::Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let api_config = config::ApiConfig::load_from_file(path)?;
        let domain_config = DomainConfig::from(api_config);
        Ok(Self::from_domain_config(domain_config))
    }
    
    /// Load service from default configuration using dependency injection
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let api_config = config::ApiConfig::load()?;
        let domain_config = DomainConfig::from(api_config);
        Ok(Self::from_domain_config(domain_config))
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
        };
        let service = ProviderService::new(config);
        assert_eq!(service.providers.len(), 0);
        assert_eq!(service.model_mapping.len(), 0);
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
        };
        
        let service = ProviderService::from_domain_config(config);
        assert_eq!(service.providers.len(), 1);
        // Note: model_mapping is empty until discover_models() is called
    }
}
