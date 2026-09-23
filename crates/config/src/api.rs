use crate::{ita::ItaAttestationConfig, types::*};
use std::env;

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub admission_cache: crate::AdmissionCacheConfig,
    /// Canonical model IDs eligible for native stateless Responses. Empty disables routing.
    pub native_responses_models: Vec<String>,
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
    /// Dedicated AES-256 key for confidential database fields. This must not
    /// reuse the object-storage encryption key.
    pub database_encryption_key: String,
    /// Identifier embedded in and validated against every database envelope.
    pub database_encryption_key_id: String,
    /// Enables encryption for newly written confidential database fields.
    /// Defaults off so dual-read support can be deployed fleet-wide first.
    pub database_encryption_write_enabled: bool,
    pub s3: S3Config,
    pub invitation_email: InvitationEmailConfig,
    pub otlp: OtlpConfig,
    pub cors: CorsConfig,
    pub external_providers: ExternalProvidersConfig,
    pub github_dispatch: GitHubDispatchConfig,
    pub infra: InfraConfig,
    pub staking_farm: StakingFarmConfig,
    pub aml: AmlConfig,
    pub usage_reporting: UsageReportingConfig,
    /// Posting-time credit allocation policy. The order is persisted with
    /// every attributed usage charge, so changing it never rewrites history.
    pub credit_allocation: CreditAllocationConfig,
    pub ita: ItaAttestationConfig,
}

impl ApiConfig {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self, String> {
        let auth = AuthConfig::from_env()?;
        Ok(Self {
            admission_cache: crate::AdmissionCacheConfig::from_env()?,
            native_responses_models: parse_native_responses_models(
                &env::var("NATIVE_RESPONSES_MODELS").unwrap_or_default(),
            ),
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
            database_encryption_key: read_required_secret_env(
                "DB_ENCRYPTION_KEY_FILE",
                "DB_ENCRYPTION_KEY",
            )?,
            database_encryption_key_id: non_empty_env("DB_ENCRYPTION_KEY_ID")
                .unwrap_or_else(|| "db-v1".to_string()),
            database_encryption_write_enabled: parse_bool_env(
                "DB_ENCRYPTION_WRITE_ENABLED",
                false,
            )?,
            s3: S3Config::from_env()?,
            invitation_email: InvitationEmailConfig::from_env()?,
            otlp: OtlpConfig::from_env()?,
            cors: CorsConfig::default(),
            external_providers: ExternalProvidersConfig::from_env(),
            github_dispatch: GitHubDispatchConfig::from_env()?,
            infra: InfraConfig::from_env()?,
            aml: AmlConfig::from_env()?,
            ita: ItaAttestationConfig::from_env()?,
            usage_reporting: UsageReportingConfig::from_env()?,
            credit_allocation: CreditAllocationConfig::from_env()?,
        })
    }
}
