use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
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
    /// Enable TLS for database connections (required for remote databases like DigitalOcean)
    /// Uses native-tls with system certificate store for verification
    #[serde(default)]
    pub tls_enabled: bool,
    /// Path to a custom CA certificate file (optional)
    /// If provided, this certificate will be added to the trust store
    #[serde(default)]
    pub tls_ca_cert_path: Option<String>,
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
    pub discovery_server_url: String,
    pub api_key: Option<String>,
    pub refresh_interval: u64, // seconds
    pub timeout: u64,          // seconds
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
pub struct DstackClientConfig {
    pub url: String,
}

// Domain-specific configuration types that will be used by domain layer
#[derive(Debug, Clone)]
pub struct DomainConfig {
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
    /// Email domains that are granted platform admin access
    /// Users with emails from these domains will have admin privileges
    #[serde(default)]
    pub admin_domains: Vec<String>,
}

impl AuthConfig {
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
            model_discovery: api_config.model_discovery,
            dstack_client: api_config.dstack_client,
            auth: api_config.auth,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_admin_email() {
        let config = AuthConfig {
            mock: false,
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
            github: None,
            google: None,
            admin_domains: vec![],
        };

        // Should return false when no admin domains configured
        assert!(!config.is_admin_email("admin@near.ai"));
    }

    #[test]
    fn test_tls_disabled_by_default() {
        let db_config = DatabaseConfig {
            host: "localhost".to_string(),
            port: 5432,
            database: "test_db".to_string(),
            username: "user".to_string(),
            password: "pass".to_string(),
            max_connections: 5,
            tls_enabled: false,
            tls_ca_cert_path: None,
        };

        assert_eq!(db_config.host, "localhost");
        assert_eq!(db_config.port, 5432);
        assert!(!db_config.tls_enabled);
    }

    #[test]
    fn test_database_config_with_tls_enabled() {
        let db_config = DatabaseConfig {
            host: "remote.example.com".to_string(),
            port: 5432,
            database: "prod_db".to_string(),
            username: "user".to_string(),
            password: "pass".to_string(),
            max_connections: 10,
            tls_enabled: true,
            tls_ca_cert_path: None,
        };

        assert_eq!(db_config.host, "remote.example.com");
        assert!(db_config.tls_enabled);
    }

    #[test]
    fn test_database_config_connection_url() {
        let db_config = DatabaseConfig {
            host: "localhost".to_string(),
            port: 5432,
            database: "mydb".to_string(),
            username: "admin".to_string(),
            password: "secret".to_string(),
            max_connections: 5,
            tls_enabled: false,
            tls_ca_cert_path: None,
        };

        let url = db_config.connection_url();
        assert_eq!(url, "postgres://admin:secret@localhost:5432/mydb");
    }

    #[test]
    fn test_database_config_yaml_deserialization_without_tls() {
        let yaml = r#"
host: "localhost"
port: 5432
database: "test_db"
username: "postgres"
password: "postgres"
max_connections: 5
"#;
        let db_config: DatabaseConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(db_config.host, "localhost");
        assert_eq!(db_config.port, 5432);
        assert!(!db_config.tls_enabled); // Should use default (false)
    }

    #[test]
    fn test_database_config_yaml_deserialization_with_tls() {
        let yaml = r#"
host: "remote.example.com"
port: 5432
database: "prod_db"
username: "dbuser"
password: "dbpass"
max_connections: 10
tls_enabled: true
"#;
        let db_config: DatabaseConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(db_config.host, "remote.example.com");
        assert!(db_config.tls_enabled);
    }

    #[test]
    fn test_database_config_yaml_serialization() {
        let db_config = DatabaseConfig {
            host: "remote.db.com".to_string(),
            port: 5432,
            database: "myapp".to_string(),
            username: "app_user".to_string(),
            password: "app_pass".to_string(),
            max_connections: 20,
            tls_enabled: true,
            tls_ca_cert_path: None,
        };

        let yaml = serde_yaml::to_string(&db_config).unwrap();
        assert!(yaml.contains("host: remote.db.com"));
        assert!(yaml.contains("tls_enabled: true"));
    }
}
