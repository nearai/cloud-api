pub mod models;
pub mod pool;
pub mod repositories;
pub mod migrations;

pub use models::*;
pub use pool::{DatabaseConfig, DbPool, create_pool};
pub use repositories::{
    OrganizationRepository,
    UserRepository,
    ApiKeyRepository,
    SessionRepository,
    McpConnectorRepository,
    ConversationRepository,
    ResponseRepository,
};

use anyhow::Result;

/// Database service combining all repositories
pub struct Database {
    pub organizations: OrganizationRepository,
    pub users: UserRepository,
    pub api_keys: ApiKeyRepository,
    pub sessions: SessionRepository,
    pub mcp_connectors: McpConnectorRepository,
    pub conversations: ConversationRepository,
    pub responses: ResponseRepository,
    pool: DbPool,
}

impl Database {
    /// Create a new database service from a connection pool
    pub fn new(pool: DbPool) -> Self {
        Self {
            organizations: OrganizationRepository::new(pool.clone()),
            users: UserRepository::new(pool.clone()),
            api_keys: ApiKeyRepository::new(pool.clone()),
            sessions: SessionRepository::new(pool.clone()),
            mcp_connectors: McpConnectorRepository::new(pool.clone()),
            conversations: ConversationRepository::new(pool.clone()),
            responses: ResponseRepository::new(pool.clone()),
            pool,
        }
    }

    /// Create a new database service from configuration
    pub async fn from_config(config: &DatabaseConfig) -> Result<Self> {
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