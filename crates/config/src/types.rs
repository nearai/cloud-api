use std::{collections::HashMap, env};

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub server: ServerConfig,
    pub model_discovery: ModelDiscoveryConfig,
    pub logging: LoggingConfig,
    pub dstack_client: DstackClientConfig,
    pub auth: AuthConfig,
    pub database: DatabaseConfig,
    pub s3: S3Config,
}

impl ApiConfig {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            server: ServerConfig::from_env()?,
            model_discovery: ModelDiscoveryConfig::from_env()?,
            logging: LoggingConfig::from_env()?,
            dstack_client: DstackClientConfig::from_env()?,
            auth: AuthConfig::from_env()?,
            database: DatabaseConfig::from_env()?,
            s3: S3Config::from_env()?,
        })
    }
}

/// Database configuration
#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    pub primary_app_id: String,
    pub host: Option<String>,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
    pub max_connections: usize,
    /// Enable TLS for database connections (required for remote databases like DigitalOcean)
    /// Uses native-tls with system certificate store for verification
    pub tls_enabled: bool,
    /// Path to a custom CA certificate file (optional)
    /// If provided, this certificate will be added to the trust store
    pub tls_ca_cert_path: Option<String>,
    /// Interval in seconds for refreshing cluster state
    pub refresh_interval: u64,
    /// Use mock database for testing (bypasses Patroni discovery and real database)
    pub mock: bool,
}

impl DatabaseConfig {
    /// Load from environment variables
    pub fn from_env() -> Result<Self, String> {
        // Password is read from a file: DATABASE_PASSWORD_FILE.
        // This file would be mounted as a secret in production deployments.
        let password = if let Ok(path) = env::var("DATABASE_PASSWORD_FILE") {
            std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read DATABASE_PASSWORD_FILE: {e}"))?
                .trim()
                .to_string()
        } else {
            env::var("DATABASE_PASSWORD").map_err(|_| "DATABASE_PASSWORD not set".to_string())?
        };
        if password.is_empty() {
            return Err("Database password cannot be empty".to_string());
        }
        Ok(Self {
            primary_app_id: env::var("POSTGRES_PRIMARY_APP_ID")
                .map_err(|_| "POSTGRES_PRIMARY_APP_ID not set".to_string())?,
            host: env::var("DATABASE_HOST").ok(),
            port: env::var("DATABASE_PORT")
                .unwrap_or_else(|_| "5432".to_string())
                .parse()
                .map_err(|_| "DATABASE_PORT must be a valid port number")?,
            database: env::var("DATABASE_NAME").map_err(|_| "DATABASE_NAME not set")?,
            username: env::var("DATABASE_USERNAME").map_err(|_| "DATABASE_USERNAME not set")?,
            max_connections: env::var("DATABASE_MAX_CONNECTIONS")
                .unwrap_or_else(|_| "16".to_string())
                .parse()
                .map_err(|_| "DATABASE_MAX_CONNECTIONS must be a valid number")?,
            tls_enabled: env::var("DATABASE_TLS_ENABLED")
                .unwrap_or_else(|_| "true".to_string())
                .parse()
                .map_err(|_| "DATABASE_TLS_ENABLED must be true or false")?,
            tls_ca_cert_path: env::var("DATABASE_TLS_CA_CERT_PATH").ok(),
            refresh_interval: env::var("DATABASE_REFRESH_INTERVAL")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .map_err(|_| "DATABASE_REFRESH_INTERVAL must be a valid number")?,
            password,
            mock: false, // Default to real database in production
        })
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

impl ServerConfig {
    /// Load from environment variables
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            host: env::var("SERVER_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: env::var("SERVER_PORT")
                .unwrap_or_else(|_| "3000".to_string())
                .parse()
                .map_err(|_| "SERVER_PORT must be a valid port number")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ModelDiscoveryConfig {
    pub discovery_server_url: String,
    pub api_key: Option<String>,
    pub refresh_interval: i64,  // seconds
    pub timeout: i64,           // seconds (for discovery requests)
    pub inference_timeout: i64, // seconds (for model inference requests)
}

impl ModelDiscoveryConfig {
    /// Load from environment variables
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            discovery_server_url: env::var("MODEL_DISCOVERY_SERVER_URL")
                .map_err(|_| "MODEL_DISCOVERY_SERVER_URL not set")?,
            api_key: env::var("MODEL_DISCOVERY_API_KEY").ok(),
            refresh_interval: env::var("MODEL_DISCOVERY_REFRESH_INTERVAL")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300), // 5 minutes
            timeout: env::var("MODEL_DISCOVERY_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30), // 30 seconds
            inference_timeout: env::var("MODEL_INFERENCE_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300), // 5 minutes
        })
    }
}

impl Default for ModelDiscoveryConfig {
    fn default() -> Self {
        Self {
            discovery_server_url: env::var("MODEL_DISCOVERY_SERVER_URL")
                .expect("MODEL_DISCOVERY_SERVER_URL environment variable is required"),
            api_key: env::var("MODEL_DISCOVERY_API_KEY").ok(),
            refresh_interval: env::var("MODEL_DISCOVERY_REFRESH_INTERVAL")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300), // 5 minutes
            timeout: env::var("MODEL_DISCOVERY_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30), // 30 seconds
            inference_timeout: env::var("MODEL_INFERENCE_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300), // 5 minutes
        }
    }
}

/// Logging Configuration
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    pub level: String,
    pub format: String,
    pub modules: HashMap<String, String>,
}

impl LoggingConfig {
    /// Load from environment variables
    pub fn from_env() -> Result<Self, String> {
        let mut modules = HashMap::new();

        // Load module-specific log levels
        if let Ok(level) = env::var("LOG_MODULE_API") {
            modules.insert("api".to_string(), level);
        }
        if let Ok(level) = env::var("LOG_MODULE_SERVICES") {
            modules.insert("services".to_string(), level);
        }
        if let Ok(level) = env::var("LOG_MODULE_DOMAIN") {
            modules.insert("domain".to_string(), level);
        }

        Ok(Self {
            level: env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string()),
            format: env::var("LOG_FORMAT").unwrap_or_else(|_| "pretty".to_string()),
            modules,
        })
    }
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

#[derive(Debug, Clone)]
pub struct DstackClientConfig {
    pub url: String,
}

impl DstackClientConfig {
    /// Load from environment variables
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            url: env::var("DSTACK_CLIENT_URL")
                .unwrap_or_else(|_| "http://localhost:8000".to_string()),
        })
    }
}

// Domain-specific configuration types that will be used by domain layer
#[derive(Debug, Clone)]
pub struct DomainConfig {
    pub model_discovery: ModelDiscoveryConfig,
    pub dstack_client: DstackClientConfig,
    pub auth: AuthConfig,
}

// Simplified Authentication Configuration
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    pub mock: bool,
    pub encoding_key: String,
    pub github: Option<GitHubOAuthConfig>,
    pub google: Option<GoogleOAuthConfig>,
    /// Email domains that are granted platform admin access
    /// Users with emails from these domains will have admin privileges
    pub admin_domains: Vec<String>,
}

impl AuthConfig {
    /// Load from environment variables
    pub fn from_env() -> Result<Self, String> {
        let github = if let (Ok(client_id), Ok(client_secret), Ok(redirect_url)) = (
            env::var("GITHUB_CLIENT_ID"),
            env::var("GITHUB_CLIENT_SECRET"),
            env::var("GITHUB_REDIRECT_URL"),
        ) {
            Some(GitHubOAuthConfig {
                client_id,
                client_secret,
                redirect_url,
            })
        } else {
            None
        };

        let google = if let (Ok(client_id), Ok(client_secret), Ok(redirect_url)) = (
            env::var("GOOGLE_CLIENT_ID"),
            env::var("GOOGLE_CLIENT_SECRET"),
            env::var("GOOGLE_REDIRECT_URL"),
        ) {
            Some(GoogleOAuthConfig {
                client_id,
                client_secret,
                redirect_url,
            })
        } else {
            None
        };

        let admin_domains = env::var("AUTH_ADMIN_DOMAINS")
            .ok()
            .map(|domains| {
                domains
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            mock: env::var("AUTH_MOCK")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(false),
            encoding_key: env::var("AUTH_ENCODING_KEY")
                .expect("AUTH_ENCODING_KEY environment variable is required"),
            github,
            google,
            admin_domains,
        })
    }

    /// Check if an email address belongs to an admin domain
    pub fn is_admin_email(&self, email: &str) -> bool {
        if self.admin_domains.is_empty() {
            return false;
        }

        // Extract domain from email (everything after @)
        if let Some(domain) = email.split('@').nth(1) {
            self.admin_domains
                .iter()
                .any(|admin_domain| domain.eq_ignore_ascii_case(admin_domain))
        } else {
            false
        }
    }
}

#[derive(Debug, Clone)]
pub struct GitHubOAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
}

#[derive(Debug, Clone)]
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
            model_discovery: api_config.model_discovery,
            dstack_client: api_config.dstack_client,
            auth: api_config.auth,
        }
    }
}

/// S3 configuration for file storage
#[derive(Debug, Clone)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub encryption_key: String,
}

impl S3Config {
    /// Load from environment variables
    pub fn from_env() -> Result<Self, String> {
        // Encryption key is read from a file: S3_ENCRYPTION_KEY_FILE.
        // This file would be mounted as a secret in production deployments.
        let encryption_key = if let Ok(path) = env::var("S3_ENCRYPTION_KEY_FILE") {
            std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read S3_ENCRYPTION_KEY_FILE: {e}"))?
                .trim()
                .to_string()
        } else {
            env::var("S3_ENCRYPTION_KEY").map_err(|_| {
                "Either S3_ENCRYPTION_KEY_FILE or S3_ENCRYPTION_KEY environment variable must be set"
                    .to_string()
            })?
        };

        if encryption_key.is_empty() {
            return Err("S3 encryption key cannot be empty".to_string());
        }

        Ok(Self {
            bucket: env::var("AWS_S3_BUCKET").map_err(|_| "AWS_S3_BUCKET not set".to_string())?,
            region: env::var("AWS_S3_REGION").map_err(|_| "AWS_S3_REGION not set".to_string())?,
            encryption_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_admin_email() {
        let config = AuthConfig {
            mock: false,
            encoding_key: "mock_encoding_key".to_string(),
            github: None,
            google: None,
            admin_domains: vec!["near.ai".to_string(), "near.org".to_string()],
        };

        // Test admin domains
        assert!(config.is_admin_email("alice@near.ai"));
        assert!(config.is_admin_email("bob@near.org"));
        assert!(config.is_admin_email("admin@NEAR.AI")); // Case insensitive

        // Test non-admin domains
        assert!(!config.is_admin_email("user@example.com"));
        assert!(!config.is_admin_email("attacker@near.ai.evil.com"));
        assert!(!config.is_admin_email("invalid-email"));
        assert!(!config.is_admin_email(""));
    }

    #[test]
    fn test_is_admin_email_empty_config() {
        let config = AuthConfig {
            mock: false,
            encoding_key: "mock_encoding_key".to_string(),
            github: None,
            google: None,
            admin_domains: vec![],
        };

        // Should return false when no admin domains configured
        assert!(!config.is_admin_email("admin@near.ai"));
    }
}
