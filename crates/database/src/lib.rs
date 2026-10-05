pub mod cluster_manager;
pub mod constants;
pub mod field_encryption;
pub mod migrations;
pub mod mock;
pub mod models;
pub mod patroni_discovery;
pub mod pool;
pub mod repositories;
pub mod shutdown_coordinator;
mod usage_reporting_indexes;

pub use constants::*;
pub use models::*;
pub use pool::DbPool;
pub use repositories::{
    ApiKeyRepository, McpConnectorRepository, OAuthStateRepository,
    OrganizationReportingTokenRepository, PgAttestationRepository, PgConversationRepository,
    PgOrganizationInvitationRepository, PgOrganizationRepository, PgResponseItemsRepository,
    PgResponseRepository, PostgresNearNonceRepository, PostgresReportingUsageSummaryRepository,
    SessionRepository, UserRepository,
};
pub use shutdown_coordinator::{ShutdownCoordinator, ShutdownStage, ShutdownStageResult};
pub use usage_reporting_indexes::ensure_usage_reporting_indexes;

use anyhow::Result;
use cluster_manager::{ClusterManager, DatabaseConfig as ClusterDbConfig, ReadPreference};
use deadpool::Runtime;
use patroni_discovery::PatroniDiscovery;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};
// Re-export mock function
use crate::pool::create_pool_with_native_tls;
pub use mock::create_mock_database;

/// Database service combining all repositories
pub struct Database {
    pub organizations: PgOrganizationRepository,
    pub users: UserRepository,
    pub api_keys: ApiKeyRepository,
    pub organization_reporting_tokens: OrganizationReportingTokenRepository,
    pub sessions: SessionRepository,
    pub mcp_connectors: McpConnectorRepository,
    pub conversations: PgConversationRepository,
    pub responses: PgResponseRepository,
    pub response_items: PgResponseItemsRepository,
    pub attestation: PgAttestationRepository,
    pool: DbPool,
    cluster_manager: Option<Arc<ClusterManager>>,
}

impl Database {
    /// Create a new database service from a connection pool
    pub fn new(pool: DbPool) -> Self {
        Self {
            organizations: PgOrganizationRepository::new(pool.clone()),
            users: UserRepository::new(pool.clone()),
            api_keys: ApiKeyRepository::new(pool.clone()),
            organization_reporting_tokens: OrganizationReportingTokenRepository::new(pool.clone()),
            sessions: SessionRepository::new(pool.clone()),
            mcp_connectors: McpConnectorRepository::new(pool.clone()),
            conversations: PgConversationRepository::new(pool.clone()),
            responses: PgResponseRepository::new(pool.clone()),
            response_items: PgResponseItemsRepository::new(pool.clone()),
            attestation: PgAttestationRepository::new(pool.clone()),
            pool,
            cluster_manager: None,
        }
    }

    /// Create a database service using direct connectivity or Patroni discovery.
    pub async fn from_config(config: &config::DatabaseConfig) -> Result<Self> {
        // If mock flag is set, use mock database
        if config.mock {
            info!("Using mock database for testing");
            return create_mock_database().await;
        }

        if config.connection_mode == config::DatabaseConnectionMode::Direct {
            info!("Initializing database with a direct PostgreSQL endpoint");
            return Self::from_direct_postgres_config(config).await;
        }

        // For tests, use simple postgres connection without Patroni
        if config.primary_app_id == "postgres-test" {
            info!("Using simple PostgreSQL connection for testing");
            return Self::from_simple_postgres_config(config).await;
        }

        info!("Initializing database with Patroni discovery");
        debug!("Primary app ID: {}", config.primary_app_id);
        info!("Refresh interval: {} seconds", config.refresh_interval);

        // Create Patroni discovery
        let discovery = Arc::new(PatroniDiscovery::new(
            config.primary_app_id.clone(),
            config.gateway_subdomain.clone(),
            config.refresh_interval,
        ));

        // Perform initial cluster discovery
        info!("Performing initial cluster discovery...");
        discovery.update_cluster_state().await?;

        if let Some(leader) = discovery.get_leader().await {
            debug!("Found leader: {} at {}", leader.name, leader.host);
        } else {
            return Err(anyhow::anyhow!(
                "No leader found in cluster during initialization"
            ));
        }

        let replicas = discovery.get_replicas().await;
        info!("Found {} replicas", replicas.len());

        // Start background refresh task
        info!("Starting cluster discovery refresh task");
        discovery.clone().start_refresh_task().await;

        // Create cluster manager
        let db_config = ClusterDbConfig {
            database: config.database.clone(),
            username: config.username.clone(),
            password: config.password.clone(),
            max_write_connections: config.max_connections as u32,
            max_read_connections: config.max_connections as u32,
            tls_enabled: config.tls_enabled,
            tls_ca_cert_path: config.tls_ca_cert_path.clone(),
        };

        let cluster_manager = Arc::new(ClusterManager::new(
            discovery,
            db_config,
            ReadPreference::LeastLag,
            Some(10000), // 10 second max lag for replicas
        ));

        // Initialize cluster manager (creates initial pools)
        info!("Initializing cluster manager...");
        cluster_manager.initialize().await?;

        // Start background tasks for leader failover handling
        info!("Starting cluster manager background tasks");
        cluster_manager.clone().start_background_tasks().await;

        // Shared write-pool handle for the repositories; clones of it follow
        // the leader across failovers because ClusterManager installs new
        // pools into this same handle.
        let pool = cluster_manager.write_pool();

        info!("Database initialization with Patroni discovery complete");

        let mut db = Self::new(pool);
        db.cluster_manager = Some(cluster_manager);
        Ok(db)
    }

    /// Run database migrations
    pub async fn run_migrations(&self) -> Result<()> {
        migrations::run(&self.pool).await
    }

    async fn from_direct_postgres_config(config: &config::DatabaseConfig) -> Result<Self> {
        let pg_config = direct_pool_config(config)?;
        let pool =
            crate::pool::create_pool_with_rustls(pg_config, config.tls_ca_cert_path.as_deref())?;
        Ok(Self::new(DbPool::new(pool)))
    }

    /// Get a reference to the connection pool
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    /// Get a reference to the cluster manager (if using Patroni)
    pub fn cluster_manager(&self) -> Option<&Arc<ClusterManager>> {
        self.cluster_manager.as_ref()
    }

    /// Shutdown the database service and coordinate cleanup
    /// The process waits up to 15 seconds for connections to gracefully close
    /// before proceeding with shutdown.
    pub async fn shutdown(&self) {
        info!("Initiating database service shutdown");
        let shutdown_start = Instant::now();

        // Step 1: Cancel background tasks
        debug!("Step 1: Cancelling background cluster tasks");
        if let Some(cluster_manager) = &self.cluster_manager {
            info!("Shutting down cluster manager and discovery tasks");
            cluster_manager.shutdown().await;
            debug!("Cluster manager and discovery tasks cancelled");
        } else {
            debug!("No cluster manager active, skipping cluster shutdown");
        }

        // Step 2: Allow active connections to drain from pool
        debug!("Step 2: Allowing active connections to return to pool");
        self.wait_for_connections().await;

        // Step 3: Close the connection pool
        debug!("Step 3: Closing connection pool");
        self.close_pool().await;

        let elapsed = shutdown_start.elapsed();
        info!(
            "Database service shutdown completed in {:.2}s",
            elapsed.as_secs_f32()
        );
    }

    /// Wait for active connections to return to the pool
    async fn wait_for_connections(&self) {
        const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);

        info!(
            "Waiting up to {:?} for active connections to return",
            DRAIN_TIMEOUT
        );
        tokio::time::sleep(DRAIN_TIMEOUT).await;
        debug!("Connection wait period completed");
    }

    /// Close the connection pool
    async fn close_pool(&self) {
        debug!("Closing connection pool resources");
        info!("Connection pool shutdown initiated");
    }

    /// Create database connection for testing without Patroni
    async fn from_simple_postgres_config(config: &config::DatabaseConfig) -> Result<Self> {
        use tokio_postgres::NoTls;

        let mut pg_config = deadpool_postgres::Config::new();
        pg_config.host = Some(
            config
                .host
                .clone()
                .unwrap_or_else(|| "localhost".to_string()),
        );
        pg_config.port = Some(config.port);
        pg_config.dbname = Some(config.database.clone());
        pg_config.user = Some(config.username.clone());
        pg_config.password = Some(config.password.clone());

        let pool = if config.tls_enabled {
            create_pool_with_native_tls(pg_config, true)?
        } else {
            pg_config.create_pool(Some(Runtime::Tokio1), NoTls)?
        };

        Ok(Self::new(DbPool::new(pool)))
    }
}

fn direct_pool_config(config: &config::DatabaseConfig) -> Result<deadpool_postgres::Config> {
    let host = config
        .host
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| anyhow::anyhow!("DATABASE_HOST is required in direct mode"))?;
    anyhow::ensure!(
        config.max_connections > 0,
        "DATABASE_MAX_CONNECTIONS must be positive"
    );
    anyhow::ensure!(
        config.tls_enabled,
        "Direct database mode requires DATABASE_TLS_ENABLED=true"
    );
    anyhow::ensure!(
        !config.database.trim().is_empty(),
        "DATABASE_NAME cannot be empty in direct mode"
    );
    anyhow::ensure!(
        !config.username.trim().is_empty(),
        "DATABASE_USERNAME cannot be empty in direct mode"
    );
    let mut pg = deadpool_postgres::Config::new();
    pg.host = Some(host.to_owned());
    pg.port = Some(config.port);
    pg.dbname = Some(config.database.clone());
    pg.user = Some(config.username.clone());
    pg.password = Some(config.password.clone());
    pg.pool = Some(deadpool_postgres::PoolConfig {
        max_size: config.max_connections,
        timeouts: deadpool_postgres::Timeouts {
            wait: Some(Duration::from_secs(5)),
            create: Some(Duration::from_secs(10)),
            recycle: Some(Duration::from_secs(5)),
        },
        queue_mode: deadpool::managed::QueueMode::Fifo,
    });
    pg.manager = Some(deadpool_postgres::ManagerConfig {
        recycling_method: deadpool_postgres::RecyclingMethod::Verified,
    });
    pg.ssl_mode = Some(deadpool_postgres::SslMode::Require);
    pg.connect_timeout = Some(Duration::from_secs(10));
    Ok(pg)
}

#[cfg(test)]
mod direct_connection_tests {
    use super::*;

    fn config() -> config::DatabaseConfig {
        config::DatabaseConfig {
            connection_mode: config::DatabaseConnectionMode::Direct,
            primary_app_id: String::new(),
            gateway_subdomain: String::new(),
            host: Some("example.us-east-1.rds.amazonaws.com".into()),
            port: 5432,
            database: "postgres".into(),
            username: "application".into(),
            password: "test-only".into(),
            max_connections: 7,
            tls_enabled: true,
            tls_ca_cert_path: None,
            refresh_interval: 30,
            mock: false,
        }
    }

    #[test]
    fn direct_pool_requires_tls_and_honors_connection_settings() {
        let source = config();
        let pool = direct_pool_config(&source).unwrap();
        assert_eq!(pool.host, source.host);
        assert_eq!(pool.port, Some(5432));
        assert_eq!(pool.dbname.as_deref(), Some("postgres"));
        assert_eq!(pool.user.as_deref(), Some("application"));
        let settings = pool.pool.unwrap();
        assert_eq!(settings.max_size, 7);
        assert_eq!(settings.timeouts.wait, Some(Duration::from_secs(5)));
        assert_eq!(settings.timeouts.create, Some(Duration::from_secs(10)));
        assert_eq!(settings.timeouts.recycle, Some(Duration::from_secs(5)));
        assert!(matches!(
            pool.manager.unwrap().recycling_method,
            deadpool_postgres::RecyclingMethod::Verified
        ));
        assert!(matches!(
            pool.ssl_mode,
            Some(deadpool_postgres::SslMode::Require)
        ));
    }

    #[test]
    fn direct_host_is_normalized_and_identifiers_are_validated() {
        let mut source = config();
        source.host = Some(" \texample.us-east-1.rds.amazonaws.com\n".into());
        assert_eq!(
            direct_pool_config(&source).unwrap().host.as_deref(),
            Some("example.us-east-1.rds.amazonaws.com")
        );
        for value in ["", " \t\n"] {
            source.database = value.into();
            assert!(direct_pool_config(&source)
                .unwrap_err()
                .to_string()
                .contains("DATABASE_NAME"));
            source.database = "postgres".into();
            source.username = value.into();
            assert!(direct_pool_config(&source)
                .unwrap_err()
                .to_string()
                .contains("DATABASE_USERNAME"));
            source.username = "application".into();
        }
        // Quoted SQL identifiers may intentionally contain spaces; do not trim them.
        source.database = " database ".into();
        source.username = " user ".into();
        let pool = direct_pool_config(&source).unwrap();
        assert_eq!(pool.dbname, source.database.into());
        assert_eq!(pool.user, source.username.into());
    }

    #[test]
    fn invalid_direct_configuration_fails_closed() {
        let mut source = config();
        source.host = None;
        assert!(direct_pool_config(&source).is_err());
        source.host = Some(" ".into());
        assert!(direct_pool_config(&source).is_err());
        source.host = Some("localhost".into());
        source.max_connections = 0;
        assert!(direct_pool_config(&source).is_err());
        source.max_connections = 1;
        source.tls_enabled = false;
        source.tls_ca_cert_path = Some("ca.pem".into());
        assert!(direct_pool_config(&source).is_err());
    }

    #[tokio::test]
    async fn direct_pool_creation_times_out_if_server_stalls_after_tcp_accept() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut source = config();
        source.host = Some("127.0.0.1".into());
        source.port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let mut pg = direct_pool_config(&source).unwrap();
        // Shorten only the test deadline, not the production configuration.
        pg.pool.as_mut().unwrap().timeouts.create = Some(Duration::from_millis(100));
        let pool = crate::pool::create_pool_with_rustls(pg, None).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), pool.get()).await;
        server.abort();
        assert!(matches!(
            result.unwrap(),
            Err(deadpool_postgres::PoolError::Timeout(
                deadpool::managed::TimeoutType::Create
            ))
        ));
    }

    #[tokio::test]
    async fn direct_mode_bypasses_patroni_and_requires_configured_ca() {
        let mut source = config();
        source.tls_ca_cert_path = Some("/nonexistent-cloud-api-test-ca.pem".into());
        let error = Database::from_config(&source).await.err().unwrap();
        assert!(error
            .to_string()
            .contains("Failed to open certificate file"));
        source.tls_ca_cert_path = None;
        source.tls_enabled = false;
        for host in [
            "example.us-east-1.rds.amazonaws.com",
            "localhost",
            "127.0.0.1",
        ] {
            source.host = Some(host.into());
            let error = Database::from_config(&source).await.err().unwrap();
            assert!(error.to_string().contains("DATABASE_TLS_ENABLED=true"));
        }
    }
}
