pub mod service;
pub mod mock;
pub mod mcp_handler;
pub mod user_service;

use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use crate::{
    errors::CompletionError,
    models::*,
    providers::StreamChunk,
};
use config::ServerConfig;
use database::Database;

pub use service::ProviderRouter;
pub use mock::{MockCompletionHandler, MockTdxHandler};
pub use mcp_handler::McpCompletionHandler;
pub use user_service::UserService;

#[async_trait]
pub trait CompletionHandler: Send + Sync {
    /// Get the name of this provider/service
    fn name(&self) -> &str;
    
    /// Check if this provider supports a given model
    fn supports_model(&self, model_id: &str) -> bool;
    
    /// Get available models from the service
    async fn get_available_models(&self) -> Result<Vec<crate::providers::ModelInfo>, CompletionError>;
    
    /// Perform a chat completion (non-streaming)
    async fn chat_completion(&self, params: ChatCompletionParams) -> Result<ChatCompletionResult, CompletionError>;
    
    /// Perform a chat completion with streaming
    async fn chat_completion_stream(&self, params: ChatCompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError>;
    
    /// Perform a text completion (non-streaming)
    async fn text_completion(&self, params: CompletionParams) -> Result<CompletionResult, CompletionError>;
    
    /// Perform a text completion with streaming
    async fn text_completion_stream(&self, params: CompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError>;
    
    /// For downcasting to concrete types
    fn as_any(&self) -> &dyn std::any::Any;
}

#[async_trait]
pub trait TdxHandler: Send + Sync {
    /// Get TDX quote and attestation information
    async fn get_quote(&self) -> Result<QuoteResponse, CompletionError>;
}

pub struct Domain {
    completion_handler: Arc<dyn CompletionHandler>,
    tdx_handler: Arc<dyn TdxHandler>,
    server_config: ServerConfig,
    pub database: Option<Arc<Database>>,
}

impl Domain {
    pub fn new() -> Self {
        Self {
            completion_handler: Arc::new(MockCompletionHandler),
            tdx_handler: Arc::new(MockTdxHandler),
            server_config: ServerConfig {
                host: "0.0.0.0".to_string(),
                port: 3000,
            },
            database: None,
        }
    }
    
    /// Get the server configuration
    pub fn server_config(&self) -> &ServerConfig {
        &self.server_config
    }
    
    /// Get available models from the completion handler
    pub async fn get_available_models(&self) -> Result<Vec<crate::providers::ModelInfo>, CompletionError> {
        self.completion_handler.get_available_models().await
    }
    
    /// Create a domain from YAML configuration
    pub async fn from_config() -> Result<Self, Box<dyn std::error::Error>> {
        // Load the full API config to get server config
        let api_config = config::ApiConfig::load()?;
        let server_config = api_config.server.clone();
        
        // Initialize database if configured
        let database = {
            let db_config = database::DatabaseConfig::default();
            match Database::from_config(&db_config).await {
                Ok(db) => {
                    // Run migrations
                    if let Err(e) = db.run_migrations().await {
                        tracing::warn!("Failed to run database migrations: {}", e);
                    }
                    tracing::info!("Database initialized successfully");
                    Some(Arc::new(db))
                }
                Err(e) => {
                    tracing::warn!("Failed to initialize database: {}. Running without database support.", e);
                    None
                }
            }
        };
        
        let mut service = ProviderRouter::load()?;
        let _tdx_handler = MockTdxHandler;
        
        if service.config.use_mock {
            tracing::info!("Using mock provider (real providers disabled in config)");
            let handler: Arc<dyn CompletionHandler> = Arc::new(MockCompletionHandler);
            // Wrap with MCP handler if database is available
            let completion_handler = if database.is_some() {
                Arc::new(McpCompletionHandler::new(handler, database.clone()))
            } else {
                handler
            };
            return Ok(Self {
                completion_handler,
                tdx_handler: Arc::new(MockTdxHandler),
                server_config,
                database,
            });
        }
        
        // Discover models from all providers
        let models = service.discover_models().await;
        
        match models {
            Ok(discovered_models) => {
                if discovered_models.is_empty() {
                    tracing::warn!("No models discovered, falling back to mock mode");
                    let handler: Arc<dyn CompletionHandler> = Arc::new(MockCompletionHandler);
                    // Wrap with MCP handler if database is available
                    let completion_handler = if database.is_some() {
                        Arc::new(McpCompletionHandler::new(handler, database.clone()))
                    } else {
                        handler
                    };
                    return Ok(Self {
                        completion_handler,
                        tdx_handler: Arc::new(MockTdxHandler),
                        server_config,
                        database,
                    });
                }
                
                tracing::info!(count = discovered_models.len(), "Models discovered successfully");
                for model in &discovered_models {
                    tracing::info!(model_id = %model.id, provider = %model.provider, "Available model");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Model discovery failed, falling back to mock mode");
                let handler: Arc<dyn CompletionHandler> = Arc::new(MockCompletionHandler);
                // Wrap with MCP handler if database is available
                let completion_handler = if database.is_some() {
                    Arc::new(McpCompletionHandler::new(handler, database.clone()))
                } else {
                    handler
                };
                return Ok(Self {
                    completion_handler,
                    tdx_handler: Arc::new(MockTdxHandler),
                    server_config,
                    database,
                });
            }
        }
        
        let handler: Arc<dyn CompletionHandler> = Arc::new(service);
        // Wrap with MCP handler if database is available
        let completion_handler = if database.is_some() {
            Arc::new(McpCompletionHandler::new(handler, database.clone()))
        } else {
            handler
        };
        
        Ok(Self {
            completion_handler,
            tdx_handler: Arc::new(MockTdxHandler),
            server_config,
            database,
        })
    }
    
    pub fn with_completion_handler(completion_handler: Arc<dyn CompletionHandler>, tdx_handler: Arc<dyn TdxHandler>) -> Self {
        Self {
            completion_handler,
            tdx_handler,
            server_config: ServerConfig {
                host: "0.0.0.0".to_string(),
                port: 3000,
            },
            database: None,
        }
    }

    pub async fn chat_completion(&self, params: ChatCompletionParams) -> Result<ChatCompletionResult, CompletionError> {
        self.completion_handler.chat_completion(params).await
    }
    
    pub async fn chat_completion_stream(&self, params: ChatCompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        self.completion_handler.chat_completion_stream(params).await
    }

    pub async fn text_completion(&self, params: CompletionParams) -> Result<CompletionResult, CompletionError> {
        self.completion_handler.text_completion(params).await
    }
    
    pub async fn text_completion_stream(&self, params: CompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        self.completion_handler.text_completion_stream(params).await
    }

    /// Get TDX quote and attestation information
    pub async fn get_quote(&self) -> Result<QuoteResponse, CompletionError> {
        self.tdx_handler.get_quote().await
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
                content: Some("Hello there!".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(100),
            temperature: Some(0.7),
            top_p: Some(1.0),
            stop_sequences: None,
            stream: None,
            tools: None,
        };

        let result = domain.chat_completion(params).await;
        assert!(result.is_ok());
        
        let completion = result.unwrap();
        assert!(matches!(completion.message.role, MessageRole::Assistant));
        assert!(completion.message.content.is_some());
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
                content: Some("Hello".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(100),
            temperature: Some(0.7),
            top_p: Some(1.0),
            stop_sequences: None,
            stream: None,
            tools: None,
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
        let service = MockCompletionHandler;
        
        // Test hello response
        let params = ChatCompletionParams {
            model_id: "test".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("Hello".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
        };
        
        let result = service.chat_completion(params).await.unwrap();
        assert!(result.message.content.as_ref().unwrap().contains("Hello! How can I assist"));
        
        // Test weather response
        let params = ChatCompletionParams {
            model_id: "test".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("What's the weather?".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
        };
        
        let result = service.chat_completion(params).await.unwrap();
        assert!(result.message.content.as_ref().unwrap().contains("weather data"));
    }
}
