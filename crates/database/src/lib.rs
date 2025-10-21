pub mod cluster_manager;
pub mod migrations;
pub mod mock;
pub mod models;
pub mod patroni_discovery;
pub mod pool;
pub mod repositories;

pub use models::*;
pub use pool::DbPool;
pub use repositories::{
    ApiKeyRepository, McpConnectorRepository, PgAttestationRepository, PgConversationRepository,
    PgOrganizationInvitationRepository, PgOrganizationRepository, PgResponseRepository,
    SessionRepository, UserRepository,
};

use anyhow::{Result};
use cluster_manager::{ClusterManager, DatabaseConfig as ClusterDbConfig, ReadPreference};
use deadpool::Runtime;
use patroni_discovery::PatroniDiscovery;
use std::sync::Arc;
use tracing::info;

// Re-export mock function
use crate::pool::create_pool_with_native_tls;
pub use mock::create_mock_database;

/// Database service combining all repositories
pub struct Database {
    pub organizations: PgOrganizationRepository,
    pub users: UserRepository,
    pub api_keys: ApiKeyRepository,
    pub sessions: SessionRepository,
    pub mcp_connectors: McpConnectorRepository,
    pub conversations: PgConversationRepository,
    pub responses: PgResponseRepository,
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
            sessions: SessionRepository::new(pool.clone()),
            mcp_connectors: McpConnectorRepository::new(pool.clone()),
            conversations: PgConversationRepository::new(pool.clone()),
            responses: PgResponseRepository::new(pool.clone()),
            attestation: PgAttestationRepository::new(pool.clone()),
            pool,
            cluster_manager: None,
        }
    }

    /// Create a new database service from configuration with Patroni discovery
    pub async fn from_config(config: &config::DatabaseConfig) -> Result<Self> {
        // If mock flag is set, use mock database
        if config.mock {
            info!("Using mock database for testing");
            return create_mock_database().await;
        }

        // For tests, use simple postgres connection without Patroni
        if config.primary_app_id == "postgres-test" {
            info!("Using simple PostgreSQL connection for testing");
            return Self::from_simple_postgres_config(config).await;
        }

        info!("Initializing database with Patroni discovery");
        info!("Primary app ID: {}", config.primary_app_id);
        info!("Refresh interval: {} seconds", config.refresh_interval);

        // Create Patroni discovery
        let discovery = Arc::new(PatroniDiscovery::new(
            config.primary_app_id.clone(),
            config.refresh_interval,
        ));

        // Perform initial cluster discovery
        info!("Performing initial cluster discovery...");
        discovery.update_cluster_state().await?;

        if let Some(leader) = discovery.get_leader().await {
            info!("Found leader: {} at {}", leader.name, leader.host);
        } else {
            return Err(anyhow::anyhow!(
                "No leader found in cluster during initialization"
            ));
        }

        let replicas = discovery.get_replicas().await;
        info!("Found {} replicas", replicas.len());

        // Start background refresh task
        info!("Starting cluster discovery refresh task");
        discovery.clone().start_refresh_task();

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
        cluster_manager.clone().start_background_tasks();

        // Get write pool to use for repositories
        let pool = cluster_manager.get_write_pool().await?;

        info!("Database initialization with Patroni discovery complete");

        let mut db = Self::new(pool);
        db.cluster_manager = Some(cluster_manager);
        Ok(db)
    }

    /// Run database migrations
    pub async fn run_migrations(&self) -> Result<()> {
        migrations::run(&self.pool).await
    }

    /// Get a reference to the connection pool
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    /// Get a reference to the cluster manager (if using Patroni)
    pub fn cluster_manager(&self) -> Option<&Arc<ClusterManager>> {
        self.cluster_manager.as_ref()
    }

    /// Create database connection for testing without Patroni
    async fn from_simple_postgres_config(config: &config::DatabaseConfig) -> Result<Self> {
        use tokio_postgres::NoTls;

        let mut pg_config = deadpool_postgres::Config::new();
        pg_config.host = Some(config.host.clone().unwrap_or_else(|| "localhost".to_string()));
        pg_config.port = Some(config.port);
        pg_config.dbname = Some(config.database.clone());
        pg_config.user = Some(config.username.clone());
        pg_config.password = Some(config.password.clone());

        let pool = if config.tls_enabled {
            create_pool_with_native_tls(pg_config, true)?
        } else {
            pg_config
                .create_pool(Some(Runtime::Tokio1), NoTls)?
        };

        Ok(Self::new(pool))
    }
}
