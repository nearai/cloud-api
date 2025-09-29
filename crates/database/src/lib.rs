pub mod migrations;
pub mod models;
pub mod pool;
pub mod repositories;

pub use models::*;
pub use pool::{create_pool, DbPool};
pub use repositories::{
    ApiKeyRepository, McpConnectorRepository, PgAttestationRepository, PgConversationRepository,
    PgOrganizationRepository, PgResponseRepository, SessionRepository, UserRepository,
};

use anyhow::Result;

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
        }
    }

    /// Create a new database service from configuration
    pub async fn from_config(config: &config::DatabaseConfig) -> Result<Self> {
        let pool = create_pool(config).await?;
        Ok(Self::new(pool))
    }

    /// Run database migrations
    pub async fn run_migrations(&self) -> Result<()> {
        migrations::run(&self.pool).await
    }

    /// Get a reference to the connection pool
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }
}
