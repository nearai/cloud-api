pub mod provider_service;

use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use crate::{
    errors::CompletionError,
    models::*,
    providers::{StreamChunk, ServerConfig},
};

pub use provider_service::ProviderService;

// ============================================================================
// Domain Services
// ============================================================================

#[async_trait]
pub trait CompletionService: Send + Sync {
    async fn chat_completion(&self, params: ChatCompletionParams) -> Result<ChatCompletionResult, CompletionError>;
    async fn chat_completion_stream(&self, params: ChatCompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError>;
    async fn text_completion(&self, params: CompletionParams) -> Result<CompletionResult, CompletionError>;
    async fn text_completion_stream(&self, params: CompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError>;
    
    /// For downcasting to concrete types
    fn as_any(&self) -> &dyn std::any::Any;
}

pub struct MockCompletionService;

#[async_trait]
impl CompletionService for MockCompletionService {
    async fn chat_completion(&self, params: ChatCompletionParams) -> Result<ChatCompletionResult, CompletionError> {
        // Validate model
        if params.model_id.is_empty() {
            return Err(CompletionError::InvalidModel("Model ID cannot be empty".to_string()));
        }

        // Validate messages
        if params.messages.is_empty() {
            return Err(CompletionError::InvalidParams("Messages cannot be empty".to_string()));
        }

        // Get the last user message to respond to
        let last_message = params.messages.last().unwrap();
        let response_content = match &last_message.content.to_lowercase() {
            content if content.contains("hello") => "Hello! How can I assist you today?".to_string(),
            content if content.contains("weather") => "I don't have access to real-time weather data, but I'd be happy to help with other questions!".to_string(),
            content if content.contains("help") => "I'm here to help! Please let me know what you need assistance with.".to_string(),
            _ => format!("I understand you're asking about: '{}'. This is a mock response from the completion service.", last_message.content),
        };

        // Mock token calculation (simple heuristic)
        let prompt_tokens = params.messages.iter()
            .map(|m| m.content.split_whitespace().count() as u32)
            .sum::<u32>() + 10; // +10 for system overhead
        let completion_tokens = response_content.split_whitespace().count() as u32;

        Ok(ChatCompletionResult {
            message: ChatMessage {
                role: MessageRole::Assistant,
                content: response_content,
                name: None,
            },
            finish_reason: FinishReason::Stop,
            usage: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            },
        })
    }

    async fn chat_completion_stream(&self, _params: ChatCompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        Err(CompletionError::InternalError("Streaming not supported in mock service".to_string()))
    }

    async fn text_completion(&self, params: CompletionParams) -> Result<CompletionResult, CompletionError> {
        // Validate model
        if params.model_id.is_empty() {
            return Err(CompletionError::InvalidModel("Model ID cannot be empty".to_string()));
        }

        // Validate prompt
        if params.prompt.is_empty() {
            return Err(CompletionError::InvalidParams("Prompt cannot be empty".to_string()));
        }

        // Generate mock completion based on prompt
        let completion_text = match &params.prompt.to_lowercase() {
            prompt if prompt.contains("once upon a time") => " there was a brave knight who embarked on an epic adventure to save the kingdom from an ancient dragon.".to_string(),
            prompt if prompt.contains("the future of ai") => " will likely involve more sophisticated reasoning capabilities, better integration with human workflows, and enhanced safety measures.".to_string(),
            prompt if prompt.contains("explain") => " in simple terms: this is a concept that can be understood by breaking it down into smaller, more manageable parts.".to_string(),
            _ => format!(" continuation of your prompt: '{}'", params.prompt),
        };

        // Mock token calculation
        let prompt_tokens = params.prompt.split_whitespace().count() as u32;
        let completion_tokens = completion_text.split_whitespace().count() as u32;

        Ok(CompletionResult {
            text: completion_text,
            finish_reason: FinishReason::Stop,
            usage: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            },
        })
    }
    
    async fn text_completion_stream(&self, _params: CompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        Err(CompletionError::InternalError("Streaming not supported in mock service".to_string()))
    }
    
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ============================================================================
// Domain Factory
// ============================================================================

pub struct Domain {
    completion_service: Arc<dyn CompletionService>,
    server_config: ServerConfig,
}

impl Domain {
    pub fn new() -> Self {
        Self {
            completion_service: Arc::new(MockCompletionService),
            server_config: ServerConfig {
                host: "0.0.0.0".to_string(),
                port: 3000,
            },
        }
    }
    
    /// Get the server configuration
    pub fn server_config(&self) -> &ServerConfig {
        &self.server_config
    }
    
    /// Get available models from the completion service
    pub async fn get_available_models(&self) -> Result<Vec<crate::providers::ModelInfo>, CompletionError> {
        if let Some(provider_service) = self.completion_service.as_any().downcast_ref::<ProviderService>() {
            provider_service.get_available_models().await
        } else {
            // Mock service - return some example models
            Ok(vec![
                crate::providers::ModelInfo {
                    id: "mock-chat-model".to_string(),
                    object: "model".to_string(),
                    created: Some(1699000000),
                    owned_by: Some("mock-provider".to_string()),
                    provider: "mock".to_string(),
                },
                crate::providers::ModelInfo {
                    id: "mock-completion-model".to_string(),
                    object: "model".to_string(),
                    created: Some(1699000000),
                    owned_by: Some("mock-provider".to_string()),
                    provider: "mock".to_string(),
                },
            ])
        }
    }
    
    /// Create a domain from YAML configuration
    pub async fn from_config() -> Result<Self, Box<dyn std::error::Error>> {
        let mut service = ProviderService::load()?;
        let server_config = service.config.server.clone();
        
        if service.config.use_mock {
            tracing::info!("Using mock provider (real providers disabled in config)");
            return Ok(Self {
                completion_service: Arc::new(MockCompletionService),
                server_config,
            });
        }
        
        // Discover models from all providers
        let models = service.discover_models().await;
        
        match models {
            Ok(discovered_models) => {
                if discovered_models.is_empty() {
                    tracing::warn!("No models discovered, falling back to mock mode");
                    return Ok(Self {
                        completion_service: Arc::new(MockCompletionService),
                        server_config,
                    });
                }
                
                tracing::info!(count = discovered_models.len(), "Models discovered successfully");
                for model in &discovered_models {
                    tracing::info!(model_id = %model.id, provider = %model.provider, "Available model");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Model discovery failed, falling back to mock mode");
                return Ok(Self {
                    completion_service: Arc::new(MockCompletionService),
                    server_config,
                });
            }
        }
        
        Ok(Self {
            completion_service: Arc::new(service),
            server_config,
        })
    }
    
    /// Create a domain from a specific config file
    pub async fn from_config_file<P: AsRef<std::path::Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let mut service = ProviderService::from_config_file(path)?;
        let server_config = service.config.server.clone();
        
        if service.config.use_mock {
            tracing::info!("Using mock provider (real providers disabled in config)");
            return Ok(Self {
                completion_service: Arc::new(MockCompletionService),
                server_config,
            });
        }
        
        // Discover models from all providers
        let models = service.discover_models().await;
        
        match models {
            Ok(discovered_models) => {
                if discovered_models.is_empty() {
                    tracing::warn!("No models discovered, falling back to mock mode");
                    return Ok(Self {
                        completion_service: Arc::new(MockCompletionService),
                        server_config,
                    });
                }
                
                tracing::info!(count = discovered_models.len(), "Models discovered successfully");
                for model in &discovered_models {
                    tracing::info!(model_id = %model.id, provider = %model.provider, "Available model");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Model discovery failed, falling back to mock mode");
                return Ok(Self {
                    completion_service: Arc::new(MockCompletionService),
                    server_config,
                });
            }
        }
        
        Ok(Self {
            completion_service: Arc::new(service),
            server_config,
        })
    }

    pub fn with_completion_service(completion_service: Arc<dyn CompletionService>) -> Self {
        Self {
            completion_service,
            server_config: ServerConfig {
                host: "0.0.0.0".to_string(),
                port: 3000,
            },
        }
    }

    pub async fn chat_completion(&self, params: ChatCompletionParams) -> Result<ChatCompletionResult, CompletionError> {
        self.completion_service.chat_completion(params).await
    }
    
    pub async fn chat_completion_stream(&self, params: ChatCompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        self.completion_service.chat_completion_stream(params).await
    }

    pub async fn text_completion(&self, params: CompletionParams) -> Result<CompletionResult, CompletionError> {
        self.completion_service.text_completion(params).await
    }
    
    pub async fn text_completion_stream(&self, params: CompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        self.completion_service.text_completion_stream(params).await
    }
}

impl Default for Domain {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_chat_completion() {
        let domain = Domain::new();
        let params = ChatCompletionParams {
            model_id: "gpt-3.5-turbo".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: "Hello there!".to_string(),
                name: None,
            }],
            max_tokens: Some(100),
            temperature: Some(0.7),
            top_p: Some(1.0),
            stop_sequences: None,
            stream: None,
        };

        let result = domain.chat_completion(params).await;
        assert!(result.is_ok());
        
        let completion = result.unwrap();
        assert!(matches!(completion.message.role, MessageRole::Assistant));
        assert!(!completion.message.content.is_empty());
        assert!(completion.usage.total_tokens > 0);
    }

    #[tokio::test]
    async fn test_text_completion() {
        let domain = Domain::new();
        let params = CompletionParams {
            model_id: "text-davinci-003".to_string(),
            prompt: "Once upon a time".to_string(),
            max_tokens: Some(50),
            temperature: Some(0.8),
            top_p: Some(1.0),
            stop_sequences: None,
            stream: None,
        };

        let result = domain.text_completion(params).await;
        assert!(result.is_ok());
        
        let completion = result.unwrap();
        assert!(!completion.text.is_empty());
        assert!(completion.usage.total_tokens > 0);
    }

    #[tokio::test]
    async fn test_invalid_model() {
        let domain = Domain::new();
        let params = ChatCompletionParams {
            model_id: "".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: "Hello".to_string(),
                name: None,
            }],
            max_tokens: Some(100),
            temperature: Some(0.7),
            top_p: Some(1.0),
            stop_sequences: None,
            stream: None,
        };

        let result = domain.chat_completion(params).await;
        assert!(result.is_err());
        
        let error = result.unwrap_err();
        assert!(matches!(error, CompletionError::InvalidModel(_)));
        
        // Test that thiserror generates proper error messages
        let error_message = error.to_string();
        assert!(error_message.starts_with("Invalid model:"));
    }

    #[tokio::test]
    async fn test_mock_service_responses() {
        let service = MockCompletionService;
        
        // Test hello response
        let params = ChatCompletionParams {
            model_id: "test".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: "Hello".to_string(),
                name: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
        };
        
        let result = service.chat_completion(params).await.unwrap();
        assert!(result.message.content.contains("Hello! How can I assist"));
        
        // Test weather response
        let params = ChatCompletionParams {
            model_id: "test".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: "What's the weather?".to_string(),
                name: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
        };
        
        let result = service.chat_completion(params).await.unwrap();
        assert!(result.message.content.contains("weather data"));
    }
}
