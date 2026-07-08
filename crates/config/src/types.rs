use std::{collections::HashMap, env};

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub server: ServerConfig,
    /// API key for authenticating with inference backends (vLLM/SGLang via inference_url)
    pub inference_api_key: Option<String>,
    /// Shared secret accepted by `POST /v1/internal/usage` from trusted
    /// reporters (e.g. inference-proxy). This is the only *API endpoint* for
    /// reporter-submitted usage (the internal inference pipeline records its
    /// own usage directly, unaffected by this). When `None`, the
    /// `/v1/internal/usage` endpoint is disabled and returns 503, so reporters
    /// cannot submit usage until an operator sets the secret.
    pub internal_usage_token: Option<String>,
    pub logging: LoggingConfig,
    pub dstack_client: DstackClientConfig,
    pub auth: AuthConfig,
    pub database: DatabaseConfig,
    pub s3: S3Config,
    pub invitation_email: InvitationEmailConfig,
    pub otlp: OtlpConfig,
    pub cors: CorsConfig,
    pub external_providers: ExternalProvidersConfig,
    pub github_dispatch: GitHubDispatchConfig,
    pub infra: InfraConfig,
    pub staking_farm: StakingFarmConfig,
    pub kyt: KytConfig,
}

impl ApiConfig {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self, String> {
        let auth = AuthConfig::from_env()?;
        Ok(Self {
            server: ServerConfig::from_env()?,
            inference_api_key: env::var("INFERENCE_API_KEY")
                .or_else(|_| env::var("MODEL_DISCOVERY_API_KEY"))
                .ok(),
            // Same env-var name on both sides (inference-proxy and
            // cloud-api). Operators set both to the same secret string;
            // unsetting either side disables the new reporting path
            // without breaking anything.
            internal_usage_token: env::var("CLOUD_API_USAGE_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            logging: LoggingConfig::from_env()?,
            dstack_client: DstackClientConfig::from_env()?,
            staking_farm: StakingFarmConfig::from_env(&auth.near),
            auth,
            database: DatabaseConfig::from_env()?,
            s3: S3Config::from_env()?,
            invitation_email: InvitationEmailConfig::from_env()?,
            otlp: OtlpConfig::from_env()?,
            cors: CorsConfig::default(),
            external_providers: ExternalProvidersConfig::from_env(),
            github_dispatch: GitHubDispatchConfig::from_env()?,
            infra: InfraConfig::from_env(),
            kyt: KytConfig::from_env()?,
        })
    }
}

/// KYT/AML provider configuration for server-side wallet risk checks.
#[derive(Debug, Clone)]
pub struct KytConfig {
    pub enabled: bool,
    pub provider: String,
    pub lukka_base_url: String,
    pub lukka_bearer_token: Option<String>,
    pub timeout_seconds: u64,
    pub retries: u32,
    pub cache_ttl_seconds: i64,
}

impl Default for KytConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "lukka".to_string(),
            lukka_base_url: "https://api.blockchain-analytics.lukka.tech".to_string(),
            lukka_bearer_token: None,
            timeout_seconds: 10,
            retries: 1,
            cache_ttl_seconds: 3600,
        }
    }
}

impl KytConfig {
    pub fn from_env() -> Result<Self, String> {
        let mut config = Self {
            enabled: env::var("KYT_ENABLED")
                .ok()
                .and_then(|value| value.parse::<bool>().ok())
                .unwrap_or(false),
            provider: env::var("KYT_PROVIDER").unwrap_or_else(|_| "lukka".to_string()),
            lukka_base_url: env::var("LUKKA_BASE_URL")
                .unwrap_or_else(|_| "https://api.blockchain-analytics.lukka.tech".to_string()),
            lukka_bearer_token: if env::var("KYT_ENABLED")
                .ok()
                .and_then(|value| value.parse::<bool>().ok())
                .unwrap_or(false)
            {
                read_optional_secret_env("LUKKA_BEARER_TOKEN_FILE", "LUKKA_BEARER_TOKEN")?
            } else {
                None
            },
            timeout_seconds: env::var("KYT_TIMEOUT_SECONDS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(10),
            retries: env::var("KYT_RETRIES")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(1),
            cache_ttl_seconds: env::var("KYT_CACHE_TTL_SECONDS")
                .ok()
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(3600),
        };

        config.provider = config.provider.to_ascii_lowercase();
        config.enabled = config.enabled
            && config.provider == "lukka"
            && config.lukka_bearer_token.is_some()
            && config.cache_ttl_seconds > 0;

        Ok(config)
    }
}

/// Global House of Stake farm configuration used to convert reward units into
/// NEAR AI Cloud credits. The feature is disabled until contract/product IDs are
/// supplied by the deployment environment.
#[derive(Debug, Clone)]
pub struct StakingFarmConfig {
    pub enabled: bool,
    pub network_id: String,
    pub contract_id: String,
    pub farm_product_id: String,
    pub farm_price_id: Option<String>,
    pub credit_nano_usd_per_reward_unit: i64,
    pub sync_staleness_seconds: i64,
}

impl Default for StakingFarmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            network_id: NEAR_DEFAULT_NETWORK_ID.to_string(),
            contract_id: String::new(),
            farm_product_id: String::new(),
            farm_price_id: None,
            credit_nano_usd_per_reward_unit: 1_000_000_000,
            sync_staleness_seconds: 300,
        }
    }
}

impl StakingFarmConfig {
    pub fn from_env(near: &NearConfig) -> Self {
        let mut config = Self {
            enabled: env::var("STAKING_FARM_ENABLED")
                .ok()
                .and_then(|value| value.parse::<bool>().ok())
                .unwrap_or(false),
            network_id: near.network_id.clone(),
            contract_id: env::var("NEAR_STAKING_CONTRACT_ID").unwrap_or_default(),
            farm_product_id: env::var("STAKING_FARM_PRODUCT_ID").unwrap_or_default(),
            farm_price_id: env::var("STAKING_FARM_PRICE_ID")
                .ok()
                .filter(|value| !value.is_empty()),
            credit_nano_usd_per_reward_unit: env::var(
                "STAKING_FARM_CREDIT_NANO_USD_PER_REWARD_UNIT",
            )
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(1_000_000_000),
            sync_staleness_seconds: env::var("STAKING_FARM_SYNC_STALENESS_SECONDS")
                .ok()
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(300),
        };

        config.enabled =
            config.enabled && !config.contract_id.is_empty() && !config.farm_product_id.is_empty();
        config
    }
}

/// Configuration for the executive "Stats" dashboard's infra burn metric.
///
/// Both values are environment-specific and intentionally have NO hardcoded
/// defaults — they are provided via deployment secrets/env only. When unset,
/// the infra-summary endpoint reports no fleet data (stale).
#[derive(Debug, Clone, Default)]
pub struct InfraConfig {
    /// Internal host-inventory endpoint. `None` when unset.
    pub machines_url: Option<String>,
    /// Flat planning cost per GPU host per month (USD). `0.0` when unset.
    pub cost_per_host_usd_month: f64,
}

impl InfraConfig {
    pub fn from_env() -> Self {
        Self {
            machines_url: env::var("INFRA_MACHINES_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            cost_per_host_usd_month: env::var("INFRA_COST_PER_HOST_USD_MONTH")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0),
        }
    }
}

/// Default `event_type` when `GITHUB_DISPATCH_EVENT_TYPE` is unset. Shared
/// between `from_env` and the `Default` impl so a `Default`-constructed config
/// never dispatches with an empty type (which no workflow listens on).
pub const DEFAULT_GITHUB_DISPATCH_EVENT_TYPE: &str = "stg_model_loaded";

/// Configuration for triggering GitHub Actions workflows after admin PATCH on
/// models. When `enabled`, a successful `PATCH /v1/admin/models` fires a
/// `repository_dispatch` event so downstream automation (validate / promote
/// pipelines) can react. Intended to be enabled only on staging cloud-api.
#[derive(Debug, Clone)]
pub struct GitHubDispatchConfig {
    pub enabled: bool,
    /// Target repo in `owner/name` form.
    pub repo: Option<String>,
    /// `event_type` field in the dispatch payload. Workflows listen on
    /// `on: repository_dispatch: types: [<event_type>]`.
    pub event_type: String,
    /// Fine-grained PAT with `actions: write` scoped to `repo` only.
    pub pat: Option<String>,
}

impl Default for GitHubDispatchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            repo: None,
            event_type: DEFAULT_GITHUB_DISPATCH_EVENT_TYPE.to_string(),
            pat: None,
        }
    }
}

impl GitHubDispatchConfig {
    pub fn from_env() -> Result<Self, String> {
        let enabled = env::var("ENABLE_GITHUB_DISPATCH")
            .ok()
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(false);

        let repo = non_empty_env("GITHUB_DISPATCH_REPO");
        let event_type = non_empty_env("GITHUB_DISPATCH_EVENT_TYPE")
            .unwrap_or_else(|| DEFAULT_GITHUB_DISPATCH_EVENT_TYPE.to_string());
        // Only read the PAT (which may touch the filesystem) when enabled. A
        // disabled instance must not fail to boot just because GITHUB_DISPATCH_PAT_FILE
        // is set in a shared env template but the secret is not mounted.
        let pat = if enabled {
            read_optional_secret_env("GITHUB_DISPATCH_PAT_FILE", "GITHUB_DISPATCH_PAT")?
        } else {
            None
        };

        if enabled {
            match repo.as_deref() {
                None => {
                    return Err(
                        "GITHUB_DISPATCH_REPO must be set when ENABLE_GITHUB_DISPATCH=true"
                            .to_string(),
                    );
                }
                Some(r) if !is_owner_name(r) => {
                    return Err(format!(
                        "GITHUB_DISPATCH_REPO must be in 'owner/name' form, got '{r}'"
                    ));
                }
                _ => {}
            }
            if pat.is_none() {
                return Err(
                    "GITHUB_DISPATCH_PAT or GITHUB_DISPATCH_PAT_FILE must be set when ENABLE_GITHUB_DISPATCH=true"
                        .to_string(),
                );
            }
        }

        Ok(Self {
            enabled,
            repo,
            event_type,
            pat,
        })
    }
}

/// `true` when `s` is `owner/name`: exactly one `/` with both sides non-empty.
fn is_owner_name(s: &str) -> bool {
    let mut parts = s.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty()
    )
}

/// Database configuration
#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    pub primary_app_id: String,
    pub gateway_subdomain: String,
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
            gateway_subdomain: env::var("GATEWAY_SUBDOMAIN")
                .map_err(|_| "GATEWAY_SUBDOMAIN not set".to_string())?,
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
    /// Interval in seconds between scheduled-pricing-change apply passes.
    /// Set to 0 to disable the background scheduler. Default: 60.
    pub pricing_change_apply_interval_secs: u64,
    /// Enable the OHTTP gateway (RFC 9458).  Set OHTTP_ENABLED=true to enable.
    pub ohttp_enabled: bool,
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
            pricing_change_apply_interval_secs: env::var("PRICING_CHANGE_APPLY_INTERVAL_SECS")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .map_err(|_| "PRICING_CHANGE_APPLY_INTERVAL_SECS must be a non-negative integer")?,
            ohttp_enabled: env::var("OHTTP_ENABLED")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
        })
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
    pub dstack_client: DstackClientConfig,
    pub auth: AuthConfig,
    pub external_providers: ExternalProvidersConfig,
}

// Simplified Authentication Configuration
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    pub mock: bool,
    pub encoding_key: String,
    pub github: Option<GitHubOAuthConfig>,
    pub google: Option<GoogleOAuthConfig>,
    pub near: NearConfig,
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

        let near = NearConfig::from_env();

        Ok(Self {
            mock: env::var("AUTH_MOCK")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(false),
            encoding_key: env::var("AUTH_ENCODING_KEY")
                .expect("AUTH_ENCODING_KEY environment variable is required"),
            github,
            google,
            near,
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

/// NEAR wallet authentication configuration
#[derive(Debug, Clone)]
pub struct NearConfig {
    pub rpc_url: String,
    pub network_id: String,
    pub expected_recipient: String,
}

const NEAR_DEFAULT_NETWORK_ID: &str = "mainnet";
const NEAR_DEFAULT_RECIPIENT: &str = "cloud.near.ai";
const NEAR_DEFAULT_RPC_URL: &str = "https://free.rpc.fastnear.com";

impl Default for NearConfig {
    fn default() -> Self {
        Self {
            rpc_url: NEAR_DEFAULT_RPC_URL.to_string(),
            network_id: NEAR_DEFAULT_NETWORK_ID.to_string(),
            expected_recipient: NEAR_DEFAULT_RECIPIENT.to_string(),
        }
    }
}

impl NearConfig {
    pub fn from_env() -> Self {
        Self {
            rpc_url: env::var("NEAR_RPC_URL").unwrap_or_else(|_| NEAR_DEFAULT_RPC_URL.to_string()),
            network_id: env::var("NEAR_NETWORK_ID")
                .unwrap_or_else(|_| NEAR_DEFAULT_NETWORK_ID.to_string()),
            expected_recipient: env::var("NEAR_EXPECTED_RECIPIENT")
                .unwrap_or_else(|_| NEAR_DEFAULT_RECIPIENT.to_string()),
        }
    }
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
            dstack_client: api_config.dstack_client,
            auth: api_config.auth,
            external_providers: api_config.external_providers,
        }
    }
}

/// S3 configuration for file storage
#[derive(Debug, Clone)]
pub struct S3Config {
    pub mock: bool,
    pub bucket: String,
    pub region: String,
    pub encryption_key: String,
}

impl S3Config {
    /// Load from environment variables
    pub fn from_env() -> Result<Self, String> {
        // Check if mock mode is enabled
        let mock = env::var("S3_MOCK")
            .ok()
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(false);

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
            mock,
            bucket: env::var("AWS_S3_BUCKET").map_err(|_| "AWS_S3_BUCKET not set".to_string())?,
            region: env::var("AWS_S3_REGION").map_err(|_| "AWS_S3_REGION not set".to_string())?,
            encryption_key,
        })
    }
}

/// Email notification configuration for organization invitations.
#[derive(Debug, Clone, Default)]
pub struct InvitationEmailConfig {
    pub enabled: bool,
    pub from_email: Option<String>,
    pub reply_to: Option<String>,
    pub resend_api_key: Option<String>,
    pub frontend_base_url: Option<String>,
}

impl InvitationEmailConfig {
    pub fn from_env() -> Result<Self, String> {
        let enabled = env::var("INVITATION_EMAIL_ENABLED")
            .ok()
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(false);

        let from_email = non_empty_env("INVITATION_EMAIL_FROM");
        let reply_to = non_empty_env("INVITATION_EMAIL_REPLY_TO");
        let resend_api_key = if enabled {
            read_optional_secret_env("RESEND_API_KEY_FILE", "RESEND_API_KEY")?
        } else {
            None
        };
        let frontend_base_url = non_empty_env("CLOUD_UI_BASE_URL");

        if enabled {
            if from_email.is_none() {
                return Err(
                    "INVITATION_EMAIL_FROM must be set when INVITATION_EMAIL_ENABLED=true"
                        .to_string(),
                );
            }
            if resend_api_key.is_none() {
                return Err(
                    "RESEND_API_KEY or RESEND_API_KEY_FILE must be set when INVITATION_EMAIL_ENABLED=true"
                        .to_string(),
                );
            }
            if frontend_base_url.is_none() {
                return Err(
                    "CLOUD_UI_BASE_URL must be set when INVITATION_EMAIL_ENABLED=true".to_string(),
                );
            }
        }

        Ok(Self {
            enabled,
            from_email,
            reply_to,
            resend_api_key,
            frontend_base_url,
        })
    }

    pub fn invitations_url(&self) -> Option<String> {
        self.frontend_base_url
            .as_ref()
            .map(|base_url| format!("{}/dashboard/invitations", base_url.trim_end_matches('/')))
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn read_optional_secret_env(file_key: &str, value_key: &str) -> Result<Option<String>, String> {
    if let Some(path) = non_empty_env(file_key) {
        let value = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {file_key}: {e}"))?
            .trim()
            .to_string();
        if value.is_empty() {
            return Err(format!("{file_key} cannot be empty"));
        }
        return Ok(Some(value));
    }

    Ok(non_empty_env(value_key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_is_admin_email() {
        let config = AuthConfig {
            mock: false,
            encoding_key: "mock_encoding_key".to_string(),
            github: None,
            google: None,
            near: NearConfig::default(),
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
            near: NearConfig::default(),
            admin_domains: vec![],
        };

        // Should return false when no admin domains configured
        assert!(!config.is_admin_email("admin@near.ai"));
    }

    fn clear_github_dispatch_env() {
        for key in [
            "ENABLE_GITHUB_DISPATCH",
            "GITHUB_DISPATCH_REPO",
            "GITHUB_DISPATCH_EVENT_TYPE",
            "GITHUB_DISPATCH_PAT",
            "GITHUB_DISPATCH_PAT_FILE",
        ] {
            std::env::remove_var(key);
        }
    }

    fn clear_staking_farm_env() {
        for key in [
            "STAKING_FARM_ENABLED",
            "NEAR_STAKING_CONTRACT_ID",
            "STAKING_FARM_PRODUCT_ID",
            "STAKING_FARM_PRICE_ID",
            "STAKING_FARM_CREDIT_NANO_USD_PER_REWARD_UNIT",
            "STAKING_FARM_SYNC_STALENESS_SECONDS",
            "NEAR_NETWORK_ID",
            "NEAR_RPC_URL",
        ] {
            std::env::remove_var(key);
        }
    }

    fn clear_kyt_env() {
        for key in [
            "KYT_ENABLED",
            "KYT_PROVIDER",
            "LUKKA_BASE_URL",
            "LUKKA_BEARER_TOKEN",
            "LUKKA_BEARER_TOKEN_FILE",
            "KYT_TIMEOUT_SECONDS",
            "KYT_RETRIES",
            "KYT_CACHE_TTL_SECONDS",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    #[serial]
    fn github_dispatch_disabled_by_default() {
        clear_github_dispatch_env();
        let config = GitHubDispatchConfig::from_env().unwrap();
        assert!(!config.enabled);
        assert!(config.repo.is_none());
        assert!(config.pat.is_none());
        assert_eq!(config.event_type, DEFAULT_GITHUB_DISPATCH_EVENT_TYPE);
    }

    #[test]
    #[serial]
    fn github_dispatch_default_matches_from_env_event_type() {
        // The Default impl must agree with from_env's fallback so a
        // Default-constructed config never carries an empty event_type.
        assert_eq!(
            GitHubDispatchConfig::default().event_type,
            DEFAULT_GITHUB_DISPATCH_EVENT_TYPE
        );
    }

    #[test]
    #[serial]
    fn staking_farm_config_uses_shared_near_network() {
        clear_staking_farm_env();
        std::env::set_var("STAKING_FARM_ENABLED", "true");
        std::env::set_var("NEAR_STAKING_CONTRACT_ID", "stake.testnet");
        std::env::set_var("STAKING_FARM_PRODUCT_ID", "cloud-credits");
        std::env::set_var("NEAR_NETWORK_ID", "testnet");
        std::env::set_var("NEAR_RPC_URL", "https://rpc.testnet.near.org");

        let near = NearConfig::from_env();
        let config = StakingFarmConfig::from_env(&near);

        assert!(config.enabled);
        assert_eq!(config.network_id, "testnet");
        assert_eq!(near.rpc_url, "https://rpc.testnet.near.org");
        assert_eq!(config.contract_id, "stake.testnet");
        assert_eq!(config.farm_product_id, "cloud-credits");

        clear_staking_farm_env();
    }

    #[test]
    #[serial]
    fn kyt_disabled_by_default() {
        clear_kyt_env();
        let config = KytConfig::from_env().unwrap();
        assert!(!config.enabled);
        assert_eq!(config.provider, "lukka");
        assert!(config.lukka_bearer_token.is_none());
    }

    #[test]
    #[serial]
    fn kyt_disabled_ignores_unreadable_token_file() {
        clear_kyt_env();
        std::env::set_var("KYT_ENABLED", "false");
        std::env::set_var("LUKKA_BEARER_TOKEN_FILE", "/missing/lukka-token");

        let config = KytConfig::from_env().unwrap();

        assert!(!config.enabled);
        assert!(config.lukka_bearer_token.is_none());
        clear_kyt_env();
    }

    #[test]
    #[serial]
    fn kyt_enabled_requires_token_to_be_effectively_enabled() {
        clear_kyt_env();
        std::env::set_var("KYT_ENABLED", "true");

        let config = KytConfig::from_env().unwrap();

        assert!(!config.enabled);
        assert!(config.lukka_bearer_token.is_none());
        clear_kyt_env();
    }

    #[test]
    #[serial]
    fn kyt_enabled_reads_inline_token() {
        clear_kyt_env();
        std::env::set_var("KYT_ENABLED", "true");
        std::env::set_var("LUKKA_BEARER_TOKEN", "lukka-secret");
        std::env::set_var("KYT_CACHE_TTL_SECONDS", "600");

        let config = KytConfig::from_env().unwrap();

        assert!(config.enabled);
        assert_eq!(config.lukka_bearer_token.as_deref(), Some("lukka-secret"));
        assert_eq!(config.cache_ttl_seconds, 600);
        clear_kyt_env();
    }

    #[test]
    #[serial]
    fn github_dispatch_disabled_ignores_unreadable_pat_file() {
        // Regression: a disabled instance must boot even when PAT_FILE points
        // at a missing secret (e.g. left in a shared env template).
        clear_github_dispatch_env();
        std::env::set_var("ENABLE_GITHUB_DISPATCH", "false");
        std::env::set_var(
            "GITHUB_DISPATCH_PAT_FILE",
            "/nonexistent/github_dispatch_pat",
        );
        let config = GitHubDispatchConfig::from_env().unwrap();
        assert!(!config.enabled);
        assert!(config.pat.is_none());
        clear_github_dispatch_env();
    }

    #[test]
    #[serial]
    fn github_dispatch_enabled_requires_repo() {
        clear_github_dispatch_env();
        std::env::set_var("ENABLE_GITHUB_DISPATCH", "true");
        std::env::set_var("GITHUB_DISPATCH_PAT", "ghp_test");
        let err = GitHubDispatchConfig::from_env().unwrap_err();
        assert!(err.contains("GITHUB_DISPATCH_REPO"));
        clear_github_dispatch_env();
    }

    #[test]
    #[serial]
    fn github_dispatch_enabled_requires_pat() {
        clear_github_dispatch_env();
        std::env::set_var("ENABLE_GITHUB_DISPATCH", "true");
        std::env::set_var("GITHUB_DISPATCH_REPO", "nearai/cvm-ansible-playbooks");
        let err = GitHubDispatchConfig::from_env().unwrap_err();
        assert!(err.contains("PAT"));
        clear_github_dispatch_env();
    }

    #[test]
    #[serial]
    fn github_dispatch_enabled_rejects_malformed_repo() {
        clear_github_dispatch_env();
        std::env::set_var("ENABLE_GITHUB_DISPATCH", "true");
        std::env::set_var("GITHUB_DISPATCH_REPO", "noslash");
        std::env::set_var("GITHUB_DISPATCH_PAT", "ghp_test");
        let err = GitHubDispatchConfig::from_env().unwrap_err();
        assert!(err.contains("owner/name"));
        clear_github_dispatch_env();
    }

    #[test]
    #[serial]
    fn github_dispatch_enabled_ok_with_inline_pat() {
        clear_github_dispatch_env();
        std::env::set_var("ENABLE_GITHUB_DISPATCH", "true");
        std::env::set_var("GITHUB_DISPATCH_REPO", "nearai/cvm-ansible-playbooks");
        std::env::set_var("GITHUB_DISPATCH_PAT", "ghp_test");
        let config = GitHubDispatchConfig::from_env().unwrap();
        assert!(config.enabled);
        assert_eq!(config.repo.as_deref(), Some("nearai/cvm-ansible-playbooks"));
        assert_eq!(config.pat.as_deref(), Some("ghp_test"));
        assert_eq!(config.event_type, DEFAULT_GITHUB_DISPATCH_EVENT_TYPE);
        clear_github_dispatch_env();
    }

    #[test]
    #[serial]
    fn test_cors_config_parsing_exact_matches() {
        std::env::set_var(
            "CORS_ALLOWED_ORIGINS",
            "https://example.com,http://test.com",
        );
        let config = CorsConfig::default();
        assert!(config
            .exact_matches
            .contains(&"https://example.com".to_string()));
        assert!(config
            .exact_matches
            .contains(&"http://test.com".to_string()));
        assert!(config.wildcard_suffixes.is_empty());
        std::env::remove_var("CORS_ALLOWED_ORIGINS");
    }

    #[test]
    #[serial]
    fn test_cors_config_parsing_wildcard_with_dot() {
        std::env::set_var("CORS_ALLOWED_ORIGINS", "*.near.ai");
        let config = CorsConfig::default();
        assert_eq!(config.wildcard_suffixes, vec![".near.ai"]);
        assert!(config.exact_matches.is_empty());
        std::env::remove_var("CORS_ALLOWED_ORIGINS");
    }

    #[test]
    #[serial]
    fn test_cors_config_parsing_wildcard_without_dot() {
        std::env::set_var("CORS_ALLOWED_ORIGINS", "*near.ai");
        let config = CorsConfig::default();
        assert_eq!(config.wildcard_suffixes, vec![".near.ai"]);
        std::env::remove_var("CORS_ALLOWED_ORIGINS");
    }

    #[test]
    #[serial]
    fn test_cors_config_parsing_wildcard_with_hyphen() {
        std::env::set_var("CORS_ALLOWED_ORIGINS", "*-example.com");
        let config = CorsConfig::default();
        assert_eq!(config.wildcard_suffixes, vec!["-example.com"]);
        std::env::remove_var("CORS_ALLOWED_ORIGINS");
    }

    #[test]
    #[serial]
    fn test_cors_config_parsing_mixed() {
        std::env::set_var(
            "CORS_ALLOWED_ORIGINS",
            "https://example.com,*.near.ai,http://test.com",
        );
        let config = CorsConfig::default();
        assert_eq!(config.exact_matches.len(), 2);
        assert!(config
            .exact_matches
            .contains(&"https://example.com".to_string()));
        assert!(config
            .exact_matches
            .contains(&"http://test.com".to_string()));
        assert_eq!(config.wildcard_suffixes, vec![".near.ai"]);
        std::env::remove_var("CORS_ALLOWED_ORIGINS");
    }

    #[test]
    #[serial]
    fn test_cors_config_parsing_whitespace() {
        std::env::set_var("CORS_ALLOWED_ORIGINS", " https://example.com , *.near.ai ");
        let config = CorsConfig::default();
        assert!(config
            .exact_matches
            .contains(&"https://example.com".to_string()));
        assert_eq!(config.wildcard_suffixes, vec![".near.ai"]);
        std::env::remove_var("CORS_ALLOWED_ORIGINS");
    }

    #[test]
    #[serial]
    fn test_cors_config_parsing_empty_entries() {
        std::env::set_var("CORS_ALLOWED_ORIGINS", "https://example.com,,*.near.ai,");
        let config = CorsConfig::default();
        assert_eq!(config.exact_matches.len(), 1);
        assert_eq!(config.wildcard_suffixes.len(), 1);
        std::env::remove_var("CORS_ALLOWED_ORIGINS");
    }

    #[test]
    #[serial]
    fn test_invitation_email_config_defaults_disabled() {
        clear_invitation_email_env();

        let config = InvitationEmailConfig::from_env().unwrap();

        assert!(!config.enabled);
        assert!(config.from_email.is_none());
        assert!(config.resend_api_key.is_none());
        assert!(config.invitations_url().is_none());
    }

    #[test]
    #[serial]
    fn test_invitation_email_config_does_not_read_resend_key_file_when_disabled() {
        clear_invitation_email_env();
        std::env::set_var("RESEND_API_KEY_FILE", "/missing/resend-api-key");

        let config = InvitationEmailConfig::from_env().unwrap();

        assert!(!config.enabled);
        assert!(config.resend_api_key.is_none());
        clear_invitation_email_env();
    }

    #[test]
    #[serial]
    fn test_invitation_email_config_requires_from_when_enabled() {
        clear_invitation_email_env();
        std::env::set_var("INVITATION_EMAIL_ENABLED", "true");
        std::env::set_var("CLOUD_UI_BASE_URL", "https://cloud.example.com");

        let error = InvitationEmailConfig::from_env().unwrap_err();

        assert!(error.contains("INVITATION_EMAIL_FROM"));
        clear_invitation_email_env();
    }

    #[test]
    #[serial]
    fn test_invitation_email_config_requires_resend_api_key_when_enabled() {
        clear_invitation_email_env();
        std::env::set_var("INVITATION_EMAIL_ENABLED", "true");
        std::env::set_var("INVITATION_EMAIL_FROM", "no-reply@example.com");
        std::env::set_var("CLOUD_UI_BASE_URL", "https://cloud.example.com");

        let error = InvitationEmailConfig::from_env().unwrap_err();

        assert!(error.contains("RESEND_API_KEY"));
        clear_invitation_email_env();
    }

    #[test]
    #[serial]
    fn test_invitation_email_config_requires_frontend_url_when_enabled() {
        clear_invitation_email_env();
        std::env::set_var("INVITATION_EMAIL_ENABLED", "true");
        std::env::set_var("INVITATION_EMAIL_FROM", "no-reply@example.com");
        std::env::set_var("RESEND_API_KEY", "re_test");

        let error = InvitationEmailConfig::from_env().unwrap_err();

        assert!(error.contains("CLOUD_UI_BASE_URL"));
        clear_invitation_email_env();
    }

    #[test]
    #[serial]
    fn test_invitation_email_config_builds_inbox_url() {
        clear_invitation_email_env();
        std::env::set_var("INVITATION_EMAIL_ENABLED", "true");
        std::env::set_var("INVITATION_EMAIL_FROM", "no-reply@example.com");
        std::env::set_var("RESEND_API_KEY", "re_test");
        std::env::set_var("CLOUD_UI_BASE_URL", "https://cloud.example.com/");

        let config = InvitationEmailConfig::from_env().unwrap();

        assert_eq!(config.resend_api_key.as_deref(), Some("re_test"));
        assert_eq!(
            config.invitations_url().as_deref(),
            Some("https://cloud.example.com/dashboard/invitations")
        );
        clear_invitation_email_env();
    }

    #[test]
    #[serial]
    fn test_invitation_email_config_reads_resend_api_key_file() {
        clear_invitation_email_env();
        let path =
            std::env::temp_dir().join(format!("cloud-api-resend-key-{}", std::process::id()));
        std::fs::write(&path, " re_file_test \n").unwrap();
        std::env::set_var("INVITATION_EMAIL_ENABLED", "true");
        std::env::set_var("INVITATION_EMAIL_FROM", "no-reply@example.com");
        std::env::set_var("RESEND_API_KEY_FILE", &path);
        std::env::set_var("CLOUD_UI_BASE_URL", "https://cloud.example.com/");

        let config = InvitationEmailConfig::from_env().unwrap();

        assert_eq!(config.resend_api_key.as_deref(), Some("re_file_test"));
        clear_invitation_email_env();
        std::fs::remove_file(path).unwrap();
    }

    fn clear_invitation_email_env() {
        std::env::remove_var("INVITATION_EMAIL_ENABLED");
        std::env::remove_var("INVITATION_EMAIL_FROM");
        std::env::remove_var("INVITATION_EMAIL_REPLY_TO");
        std::env::remove_var("RESEND_API_KEY");
        std::env::remove_var("RESEND_API_KEY_FILE");
        std::env::remove_var("CLOUD_UI_BASE_URL");
    }

    #[test]
    #[serial]
    fn chutes_models_parses_canonical_slug_pairs_and_bare_entries() {
        // `canonical=chute_slug` pairs; a bare entry means canonical == slug.
        // Surrounding whitespace is trimmed; empty/half-empty tokens dropped.
        std::env::set_var(
            "CHUTES_MODELS",
            "zai-org/GLM-5.1-FP8=zai-org/GLM-5.1-TEE , moonshotai/Kimi-K2.6-TEE ,, =bad, alsobad=",
        );
        let cfg = ExternalProvidersConfig::from_env();
        std::env::remove_var("CHUTES_MODELS");

        assert_eq!(
            cfg.chutes_models,
            vec![
                ChutesModelEntry {
                    canonical_id: "zai-org/GLM-5.1-FP8".to_string(),
                    chute_slug: "zai-org/GLM-5.1-TEE".to_string(),
                },
                // Bare entry: canonical == slug.
                ChutesModelEntry {
                    canonical_id: "moonshotai/Kimi-K2.6-TEE".to_string(),
                    chute_slug: "moonshotai/Kimi-K2.6-TEE".to_string(),
                },
            ],
            "pairs split on '='; bare => canonical==slug; empty/half-empty tokens dropped"
        );
    }

    #[test]
    #[serial]
    fn chutes_models_empty_when_unset() {
        std::env::remove_var("CHUTES_MODELS");
        let cfg = ExternalProvidersConfig::from_env();
        assert!(cfg.chutes_models.is_empty());
    }

    #[test]
    #[serial]
    fn chutes_models_dedup_duplicate_canonical_ids_first_wins() {
        // Duplicate canonical id (even with a different slug) is dropped so a
        // misconfig can't register redundant fallback providers — first wins.
        std::env::set_var(
            "CHUTES_MODELS",
            "zai-org/GLM-5.1-FP8=zai-org/GLM-5.1-TEE,zai-org/GLM-5.1-FP8=zai-org/GLM-5-TEE,other/model",
        );
        let cfg = ExternalProvidersConfig::from_env();
        std::env::remove_var("CHUTES_MODELS");

        assert_eq!(
            cfg.chutes_models,
            vec![
                ChutesModelEntry {
                    canonical_id: "zai-org/GLM-5.1-FP8".to_string(),
                    chute_slug: "zai-org/GLM-5.1-TEE".to_string(),
                },
                ChutesModelEntry {
                    canonical_id: "other/model".to_string(),
                    chute_slug: "other/model".to_string(),
                },
            ],
            "duplicate canonical id dropped (first wins); the second slug is ignored"
        );
    }
}

/// One Chutes model to register, parsed from a single `CHUTES_MODELS` token.
///
/// The token is `canonical_id=chute_slug` (e.g. `zai-org/GLM-5.1-FP8=zai-org/GLM-5.1-TEE`),
/// or a bare `name` meaning `canonical_id == chute_slug`. We deliberately keep
/// the two ids distinct: the **canonical id** is what we expose in `/v1/models`
/// and route under (the NEAR-served id when NEAR also serves the model, else the
/// OpenRouter id) — never the raw `-TEE` chute slug; the **chute slug** is the
/// internal upstream identity we send to Chutes and resolve to a `chute_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChutesModelEntry {
    /// User-facing / catalog model id (e.g. `zai-org/GLM-5.1-FP8`).
    pub canonical_id: String,
    /// Chutes chute slug sent upstream (e.g. `zai-org/GLM-5.1-TEE`).
    pub chute_slug: String,
}

/// External providers configuration for third-party AI providers
/// API keys are loaded from environment variables or secret files
#[derive(Debug, Clone, Default)]
pub struct ExternalProvidersConfig {
    /// OpenAI API key (for OpenAI-compatible providers)
    pub openai_api_key: Option<String>,
    /// Anthropic API key
    pub anthropic_api_key: Option<String>,
    /// Google Gemini API key
    pub gemini_api_key: Option<String>,
    /// Default timeout for external provider requests (seconds)
    pub timeout_seconds: i64,
    /// Interval in seconds for refreshing external providers from the database.
    /// Set to 0 to disable periodic refresh. Default: 900 (15 minutes) in production.
    pub refresh_interval_secs: u64,
    /// Chutes attested provider — hard-off by default (`ENABLE_CHUTES`).
    pub enable_chutes: bool,
    /// Chutes API key (`cpk_...`), from `CHUTES_API_KEY[_FILE]`. A secret.
    pub chutes_api_key: Option<String>,
    /// Chutes models to register, from `CHUTES_MODELS` (comma-separated
    /// `canonical_id=chute_slug` pairs; a bare entry means the two are equal).
    /// See [`ChutesModelEntry`].
    pub chutes_models: Vec<ChutesModelEntry>,
    /// Expose Chutes **streaming** as an attested path (`CHUTES_ENABLE_STREAMING`,
    /// default off). Off because Chutes' stream protocol has no authenticated
    /// frame sequence numbers, so an on-path gateway could drop/reorder frames
    /// undetectably — non-streaming is the honest attested default until Chutes
    /// adds sequencing (and the inner-terminator behavior is verified on staging).
    pub chutes_enable_streaming: bool,
    /// Intel PCCS URL for DCAP collateral (shared with the NEAR attestation
    /// verifier), from `PCCS_URL`. One source of truth instead of ad-hoc env reads.
    pub pccs_url: Option<String>,
}

impl ExternalProvidersConfig {
    /// Load from environment variables
    /// Keys can be provided directly via env vars or through file paths
    pub fn from_env() -> Self {
        // OpenAI API key
        let openai_api_key = if let Ok(path) = env::var("OPENAI_API_KEY_FILE") {
            std::fs::read_to_string(path)
                .ok()
                .map(|s| s.trim().to_string())
        } else {
            env::var("OPENAI_API_KEY").ok()
        };

        // Anthropic API key
        let anthropic_api_key = if let Ok(path) = env::var("ANTHROPIC_API_KEY_FILE") {
            std::fs::read_to_string(path)
                .ok()
                .map(|s| s.trim().to_string())
        } else {
            env::var("ANTHROPIC_API_KEY").ok()
        };

        // Gemini API key
        let gemini_api_key = if let Ok(path) = env::var("GEMINI_API_KEY_FILE") {
            std::fs::read_to_string(path)
                .ok()
                .map(|s| s.trim().to_string())
        } else {
            env::var("GEMINI_API_KEY").ok()
        };

        // Timeout (default 5 minutes)
        let timeout_seconds = env::var("EXTERNAL_PROVIDER_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300);

        // Refresh interval for external providers (default 5 minutes)
        let refresh_interval_secs = env::var("EXTERNAL_PROVIDER_REFRESH_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300);

        // Chutes attested provider — hard-off by default.
        let enable_chutes = env::var("ENABLE_CHUTES")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let chutes_api_key = if let Ok(path) = env::var("CHUTES_API_KEY_FILE") {
            match std::fs::read_to_string(&path) {
                Ok(s) => Some(s.trim().to_string()),
                Err(e) => {
                    // Path only — never the key contents.
                    eprintln!("WARN: failed to read CHUTES_API_KEY_FILE ({path}): {e}");
                    None
                }
            }
        } else {
            env::var("CHUTES_API_KEY").ok()
        }
        // An empty key is not a key — treat "" as absent so a misconfigured
        // secret can't pass as Some("") and silently fail at request time.
        .filter(|s| !s.is_empty());
        // `canonical_id=chute_slug` per comma-separated token; a bare token means
        // canonical_id == chute_slug. Drop tokens missing either side, and dedup by
        // canonical id (first wins) so a misconfig can't register duplicate
        // fallback providers that every refresh would re-attach.
        let chutes_models = {
            let raw = env::var("CHUTES_MODELS").unwrap_or_default();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut entries: Vec<ChutesModelEntry> = Vec::new();
            for tok in raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                let entry = match tok.split_once('=') {
                    Some((canonical, slug)) => ChutesModelEntry {
                        canonical_id: canonical.trim().to_string(),
                        chute_slug: slug.trim().to_string(),
                    },
                    None => ChutesModelEntry {
                        canonical_id: tok.to_string(),
                        chute_slug: tok.to_string(),
                    },
                };
                if entry.canonical_id.is_empty() || entry.chute_slug.is_empty() {
                    continue;
                }
                if !seen.insert(entry.canonical_id.clone()) {
                    // `eprintln!` (not `tracing::warn!`) on purpose: the `config`
                    // crate has no `tracing` dependency, and `from_env` runs during
                    // startup config parsing — potentially before the tracing
                    // subscriber is installed, where a `tracing::warn!` would be
                    // dropped. stderr is always captured by the container log
                    // pipeline. Consistent with the sibling CHUTES_API_KEY_FILE warn.
                    eprintln!(
                        "WARN: duplicate CHUTES_MODELS canonical id '{}' ignored (first wins)",
                        entry.canonical_id
                    );
                    continue;
                }
                entries.push(entry);
            }
            entries
        };
        let chutes_enable_streaming = env::var("CHUTES_ENABLE_STREAMING")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let pccs_url = env::var("PCCS_URL").ok().filter(|s| !s.is_empty());

        Self {
            openai_api_key,
            anthropic_api_key,
            gemini_api_key,
            timeout_seconds,
            refresh_interval_secs,
            enable_chutes,
            chutes_api_key,
            chutes_models,
            chutes_enable_streaming,
            pccs_url,
        }
    }

    /// Get API key for a specific backend type
    pub fn get_api_key(&self, backend_type: &str) -> Option<&str> {
        match backend_type {
            "openai_compatible" => self.openai_api_key.as_deref(),
            "anthropic" => self.anthropic_api_key.as_deref(),
            "gemini" => self.gemini_api_key.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OtlpConfig {
    pub endpoint: String,
    pub protocol: String,
}

impl OtlpConfig {
    pub fn from_env() -> Result<Self, String> {
        Ok(Self::default())
    }
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            endpoint: env::var("TELEMETRY_OTLP_ENDPOINT")
                .unwrap_or_else(|_| "http://localhost:4317".to_string()),
            protocol: env::var("TELEMETRY_OTLP_PROTOCOL").unwrap_or_else(|_| "grpc".to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CorsConfig {
    pub exact_matches: Vec<String>,
    pub wildcard_suffixes: Vec<String>,
}

impl Default for CorsConfig {
    fn default() -> Self {
        let raw_origins = env::var("CORS_ALLOWED_ORIGINS")
            .unwrap_or_else(|_| "http://localhost:3000,https://near.ai,*.near.ai".to_string());

        let mut exact_matches = Vec::new();
        let mut wildcard_suffixes = Vec::new();

        for origin in raw_origins.split(',') {
            let s = origin.trim();
            if s.is_empty() {
                continue;
            }

            if let Some(suffix) = s.strip_prefix('*') {
                let safe_suffix = if suffix.starts_with('.') || suffix.starts_with('-') {
                    suffix.to_string()
                } else {
                    format!(".{suffix}")
                };
                wildcard_suffixes.push(safe_suffix);
            } else {
                exact_matches.push(s.to_string());
            }
        }

        Self {
            exact_matches,
            wildcard_suffixes,
        }
    }
}
