pub mod vllm;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::pin::Pin;
use crate::{
    errors::CompletionError,
    models::*,
};

// ============================================================================
// Provider Traits
// ============================================================================

/// Trait for LLM providers that support both streaming and non-streaming completions
#[async_trait]
pub trait CompletionProvider: Send + Sync {
    /// Get the name of this provider
    fn name(&self) -> &str;
    
    /// Get available models from this provider
    async fn get_models(&self) -> Result<Vec<ModelInfo>, CompletionError>;
    
    /// Check if this provider supports a given model
    fn supports_model(&self, model_id: &str) -> bool;
    
    /// Perform a chat completion (non-streaming)
    async fn chat_completion(
        &self,
        params: ChatCompletionParams,
    ) -> Result<ChatCompletionResult, CompletionError>;
    
    /// Perform a chat completion with streaming
    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError>;
    
    /// Perform a text completion (non-streaming)
    async fn text_completion(
        &self,
        params: CompletionParams,
    ) -> Result<CompletionResult, CompletionError>;
    
    /// Perform a text completion with streaming
    async fn text_completion_stream(
        &self,
        params: CompletionParams,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError>;
}

// ============================================================================
// Streaming Models
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<StreamChoice>,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChoice {
    pub index: u32,
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delta {
    pub role: Option<String>,
    pub content: Option<String>,
}

// ============================================================================
// Model Discovery
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub object: String,
    pub created: Option<u64>,
    pub owned_by: Option<String>,
    pub provider: String,  // Which provider serves this model
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelInfo>,
}

// ============================================================================
// Configuration Structures (for YAML)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    pub use_mock: bool,
    pub providers: Vec<ProviderConfig>,
    pub server: ServerConfig,
    pub model_discovery: ModelDiscoveryConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDiscoveryConfig {
    pub refresh_interval: u64,  // seconds
    pub timeout: u64,          // seconds
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub format: String,
    pub modules: std::collections::HashMap<String, String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        let mut modules = std::collections::HashMap::new();
        modules.insert("api".to_string(), "debug".to_string());
        modules.insert("domain".to_string(), "debug".to_string());
        
        Self {
            level: "info".to_string(),
            format: "pretty".to_string(),
            modules,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,  // "vllm", "openai", "anthropic", etc.
    pub url: String,
    pub api_key: Option<String>,
    pub enabled: bool,
    pub priority: u32,
}

// Legacy enum for internal use (converted from YAML config)
#[derive(Debug, Clone)]
pub enum ProviderType {
    VLlm {
        base_url: String,
        api_key: Option<String>,
    },
    OpenAI {
        api_key: String,
    },
    Anthropic {
        api_key: String,
    },
    Mock,
}

impl ApiConfig {
    /// Load configuration from YAML file
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: ApiConfig = serde_yaml::from_str(&content)?;
        Ok(config)
    }
    
    /// Load configuration from default locations
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        // Try different config locations in order
        let config_paths = [
            "config/config.yaml",
            "config.yaml",
            "config/default.yaml",
        ];
        
        for path in &config_paths {
            if std::path::Path::new(path).exists() {
                return Self::load_from_file(path);
            }
        }
        
        // If no config file found, fail with descriptive error
        Err(format!(
            "Configuration file not found. Tried paths: {}. Please provide a valid config file.",
            config_paths.join(", ")
        ).into())
    }
}
