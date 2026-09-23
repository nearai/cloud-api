use super::*;

pub fn test_config() -> ApiConfig {
    let _ = dotenvy::dotenv();
    let db_name = db_setup::get_test_db_name();
    ApiConfig {
        admission_cache: config::AdmissionCacheConfig::default(),
        server: config::ServerConfig {
            host: std::env::var("SERVER_HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
            port: std::env::var("SERVER_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(0),
            // Tests drive the pricing scheduler's run_once() directly.
            pricing_change_apply_interval_secs: 0,
            ohttp_enabled: false,
        },
        inference_api_key: std::env::var("INFERENCE_API_KEY")
            .or_else(|_| std::env::var("MODEL_DISCOVERY_API_KEY"))
            .ok()
            .or(Some("test_api_key".to_string())),
        internal_usage_token: None,
        native_responses_models: Vec::new(),
        logging: config::LoggingConfig {
            level: "debug".to_string(),
            format: "compact".to_string(),
            modules: std::collections::HashMap::new(),
        },
        dstack_client: config::DstackClientConfig {
            url: std::env::var("DSTACK_CLIENT_URL")
                .unwrap_or_else(|_| "http://localhost:8000".to_string()),
        },
        auth: config::AuthConfig {
            mock: true,
            encoding_key: "mock_encoding_key".to_string(),
            github: None,
            google: None,
            near: config::NearConfig::default(),
            admin_domains: vec!["test.com".to_string()],
            admin_read_only_tokens_enabled: false,
            require_session_bound_access_tokens: false,
        },
        database: config::DatabaseConfig {
            connection_mode: config::DatabaseConnectionMode::Patroni,
            primary_app_id: "postgres-test".to_string(),
            gateway_subdomain: "cvm1.near.ai".to_string(),
            port: std::env::var("DATABASE_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            host: std::env::var("DATABASE_HOST").ok(),
            database: db_name,
            username: std::env::var("DATABASE_USERNAME").unwrap_or_else(|_| "postgres".to_string()),
            password: std::env::var("DATABASE_PASSWORD").unwrap_or_else(|_| "postgres".to_string()),
            max_connections: 4,
            tls_enabled: false,
            tls_ca_cert_path: None,
            refresh_interval: 30,
            mock: false,
        },
        // Test-only deterministic keys keep local .env values from changing
        // mock-storage and database-encryption behavior across processes.
        database_encryption_key: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
            .to_string(),
        database_encryption_key_id: "e2e-db-v1".to_string(),
        // Preserve encrypted-write coverage in E2E tests; production defaults off.
        database_encryption_write_enabled: true,
        s3: config::S3Config {
            mock: true,
            bucket: "test-bucket".to_string(),
            region: "us-east-1".to_string(),
            encryption_key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
        },
        invitation_email: config::InvitationEmailConfig::default(),
        otlp: config::OtlpConfig {
            endpoint: std::env::var("TELEMETRY_OTLP_ENDPOINT")
                .unwrap_or_else(|_| "http://localhost:4317".to_string()),
            protocol: std::env::var("TELEMETRY_OTLP_PROTOCOL").unwrap_or("grpc".to_string()),
            instance_id: None,
        },
        cors: config::CorsConfig::default(),
        external_providers: config::ExternalProvidersConfig::default(),
        github_dispatch: config::GitHubDispatchConfig::default(),
        infra: config::InfraConfig::default(),
        staking_farm: config::StakingFarmConfig::default(),
        aml: config::AmlConfig::default(),
        usage_reporting: config::UsageReportingConfig {
            enabled: true,
            ..config::UsageReportingConfig::default()
        },
        credit_allocation: config::CreditAllocationConfig::default(),
        ita: config::ItaAttestationConfig::default(),
    }
}
