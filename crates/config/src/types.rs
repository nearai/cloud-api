use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    pub use_mock: bool,
    pub providers: Vec<ProviderConfig>,
    pub server: ServerConfig,
    pub model_discovery: ModelDiscoveryConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    pub dstack_client: DstackClientConfig,
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
    pub modules: HashMap<String, String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        let mut modules = HashMap::new();
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DstackClientConfig {
    pub url: String,
}

// Domain-specific configuration types that will be used by domain layer
#[derive(Debug, Clone)]
pub struct DomainConfig {
    pub use_mock: bool,
    pub providers: Vec<ProviderConfig>,
    pub model_discovery: ModelDiscoveryConfig,
    pub dstack_client: DstackClientConfig,
}

impl From<ApiConfig> for DomainConfig {
    fn from(api_config: ApiConfig) -> Self {
        Self {
            use_mock: api_config.use_mock,
            providers: api_config.providers,
            model_discovery: api_config.model_discovery,
            dstack_client: api_config.dstack_client,
        }
    }
}
