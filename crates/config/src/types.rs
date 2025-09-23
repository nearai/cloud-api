use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    pub providers: Vec<ProviderConfig>,
    pub server: ServerConfig,
    pub model_discovery: ModelDiscoveryConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    pub dstack_client: DstackClientConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    pub database: DatabaseConfig,
}

/// Database configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
    pub max_connections: usize,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            host: env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string()),
            port: env::var("DB_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: env::var("DB_NAME").unwrap_or_else(|_| "platform_api".to_string()),
            username: env::var("DB_USER").unwrap_or_else(|_| "postgres".to_string()),
            password: env::var("DB_PASSWORD").unwrap_or_else(|_| "postgres".to_string()),
            max_connections: env::var("DB_MAX_CONNECTIONS")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(20),
        }
    }
}

impl DatabaseConfig {
    /// Create a connection URL for this database configuration
    pub fn connection_url(&self) -> String {
        format!(
            "postgres://{}:{}@{}:{}/{}",
            self.username, self.password, self.host, self.port, self.database
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDiscoveryConfig {
    pub refresh_interval: u64, // seconds
    pub timeout: u64,          // seconds
}

impl Default for ModelDiscoveryConfig {
    fn default() -> Self {
        Self {
            refresh_interval: env::var("MODEL_DISCOVERY_REFRESH_INTERVAL")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300), // 5 minutes
            timeout: env::var("MODEL_DISCOVERY_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30), // 30 seconds
        }
    }
}

/// Logging Configuration
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
    pub provider_type: String, // "vllm", "redpill"
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
    pub providers: Vec<ProviderConfig>,
    pub model_discovery: ModelDiscoveryConfig,
    pub dstack_client: DstackClientConfig,
    pub auth: AuthConfig,
}

// Simplified Authentication Configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthConfig {
    pub mock: bool,
    #[serde(default)]
    pub github: Option<GitHubOAuthConfig>,
    #[serde(default)]
    pub google: Option<GoogleOAuthConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubOAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleOAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
}

// Generic OAuth provider config for unified handling
#[derive(Debug, Clone)]
pub struct OAuthProviderConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

impl From<GitHubOAuthConfig> for OAuthProviderConfig {
    fn from(config: GitHubOAuthConfig) -> Self {
        Self {
            client_id: config.client_id,
            client_secret: config.client_secret,
            redirect_uri: config.redirect_url,
        }
    }
}

impl From<GoogleOAuthConfig> for OAuthProviderConfig {
    fn from(config: GoogleOAuthConfig) -> Self {
        Self {
            client_id: config.client_id,
            client_secret: config.client_secret,
            redirect_uri: config.redirect_url,
        }
    }
}

impl From<ApiConfig> for DomainConfig {
    fn from(api_config: ApiConfig) -> Self {
        Self {
            providers: api_config.providers,
            model_discovery: api_config.model_discovery,
            dstack_client: api_config.dstack_client,
            auth: api_config.auth,
        }
    }
}
