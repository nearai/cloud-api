use crate::patroni_discovery::{ClusterMember, PatroniDiscovery};
use crate::pool::create_pool_with_native_tls;
use anyhow::{anyhow, Result};
use deadpool::managed::QueueMode;
use deadpool_postgres::{Config, Object as PooledConnection, Pool, Runtime};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time;
use tracing::{debug, error, info, warn};

/// Read preference strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPreference {
    /// Round-robin across all replicas
    RoundRobin,
    /// Choose replica with least replication lag
    LeastLag,
    /// Always use the leader for reads
    LeaderOnly,
}

impl From<&str> for ReadPreference {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "round_robin" => ReadPreference::RoundRobin,
            "least_lag" => ReadPreference::LeastLag,
            "leader_only" => ReadPreference::LeaderOnly,
            _ => ReadPreference::LeastLag,
        }
    }
}

pub struct ClusterManager {
    discovery: Arc<PatroniDiscovery>,
    write_pool: Arc<RwLock<Option<Pool>>>,
    read_pools: Arc<RwLock<HashMap<String, Pool>>>,
    database_config: DatabaseConfig,
    read_preference: ReadPreference,
    max_replica_lag_ms: Option<i64>,
    round_robin_counter: AtomicUsize,
}

#[derive(Clone)]
pub struct DatabaseConfig {
    pub database: String,
    pub username: String,
    pub password: String,
    pub max_write_connections: u32,
    pub max_read_connections: u32,
    pub tls_enabled: bool,
    pub tls_ca_cert_path: Option<String>,
}

impl ClusterManager {
    pub fn new(
        discovery: Arc<PatroniDiscovery>,
        database_config: DatabaseConfig,
        read_preference: ReadPreference,
        max_replica_lag_ms: Option<i64>,
    ) -> Self {
        Self {
            discovery,
            write_pool: Arc::new(RwLock::new(None)),
            read_pools: Arc::new(RwLock::new(HashMap::new())),
            database_config,
            read_preference,
            max_replica_lag_ms,
            round_robin_counter: AtomicUsize::new(0),
        }
    }

    /// Initialize the cluster manager and create initial pools
    pub async fn initialize(&self) -> Result<()> {
        info!("Initializing cluster manager");

        // Perform initial discovery
        self.discovery.update_cluster_state().await?;

        // Create write pool for leader
        if let Some(leader) = self.discovery.get_leader().await {
            self.create_write_pool(&leader).await?;
        } else {
            return Err(anyhow!("No leader found during initialization"));
        }

        // Create read pools for replicas
        self.update_read_pools().await?;

        info!("Cluster manager initialized successfully");
        Ok(())
    }

    /// Create a connection pool for the leader
    async fn create_write_pool(&self, leader: &ClusterMember) -> Result<()> {
        info!(
            "Creating write pool for leader: {} ({}:{})",
            leader.name, leader.host, leader.port
        );

        let pool = self.create_pool(
            &leader.host,
            leader.port,
            self.database_config.max_write_connections,
        )?;

        // Test the connection
        let conn = pool.get().await?;
        conn.simple_query("SELECT 1").await?;

        // Verify this is actually the leader
        let rows = conn.query("SELECT pg_is_in_recovery()", &[]).await?;
        let is_replica: bool = rows[0].get(0);
        if is_replica {
            warn!(
                "Node {} claims to be leader but is in recovery mode",
                leader.name
            );
        }

        let mut write_pool = self.write_pool.write().await;
        *write_pool = Some(pool);

        info!("Write pool created successfully for leader {}", leader.name);
        Ok(())
    }

    /// Update read pools based on current replicas
    async fn update_read_pools(&self) -> Result<()> {
        let replicas = self.discovery.get_replicas().await;
        let mut read_pools = self.read_pools.write().await;

        // Remove pools for replicas that no longer exist
        read_pools.retain(|host, _| replicas.iter().any(|r| &r.host == host));

        // Add pools for new replicas
        for replica in replicas {
            if !read_pools.contains_key(&replica.host) {
                info!(
                    "Creating read pool for replica: {} ({}:{})",
                    replica.name, replica.host, replica.port
                );

                match self.create_pool(
                    &replica.host,
                    replica.port,
                    self.database_config.max_read_connections,
                ) {
                    Ok(pool) => {
                        read_pools.insert(replica.host.clone(), pool);
                        info!("Read pool created for replica {}", replica.name);
                    }
                    Err(e) => {
                        error!("Failed to create pool for replica {}: {}", replica.name, e);
                    }
                }
            }
        }

        info!("Read pools updated: {} replicas", read_pools.len());
        Ok(())
    }

    /// Create a connection pool for a specific host
    fn create_pool(&self, host: &str, port: u16, max_connections: u32) -> Result<Pool> {
        let mut cfg = Config::new();
        cfg.host = Some(host.to_string());
        cfg.port = Some(port);
        cfg.dbname = Some(self.database_config.database.clone());
        cfg.user = Some(self.database_config.username.clone());
        cfg.password = Some(self.database_config.password.clone());
        cfg.pool = Some(deadpool_postgres::PoolConfig {
            max_size: max_connections as usize,
            timeouts: deadpool_postgres::Timeouts {
                wait: Some(Duration::from_secs(5)),
                create: Some(Duration::from_secs(5)),
                recycle: Some(Duration::from_secs(5)),
            },
            queue_mode: QueueMode::Fifo,
        });

        if self.database_config.tls_enabled {
            // Use native TLS and accept self-signed certificates for Patroni
            create_pool_with_native_tls(cfg, true)
        } else {
            use tokio_postgres::NoTls;
            cfg.create_pool(Some(Runtime::Tokio1), NoTls)
                .map_err(|e| anyhow!("Failed to create pool: {e}"))
        }
    }

    /// Get a connection for write operations (always uses leader)
    pub async fn get_write_connection(&self) -> Result<PooledConnection> {
        let write_pool = self.write_pool.read().await;
        let pool = write_pool
            .as_ref()
            .ok_or_else(|| anyhow!("No write pool available"))?;

        pool.get()
            .await
            .map_err(|e| anyhow!("Failed to get write connection: {e}"))
    }

    /// Get a connection for read operations (uses replicas if available)
    pub async fn get_read_connection(&self) -> Result<PooledConnection> {
        match self.read_preference {
            ReadPreference::LeaderOnly => self.get_write_connection().await,
            ReadPreference::RoundRobin => self.get_read_connection_round_robin().await,
            ReadPreference::LeastLag => self.get_read_connection_least_lag().await,
        }
    }

    /// Get read connection using round-robin selection
    async fn get_read_connection_round_robin(&self) -> Result<PooledConnection> {
        let read_pools = self.read_pools.read().await;

        if read_pools.is_empty() {
            debug!("No read replicas available, falling back to leader");
            return self.get_write_connection().await;
        }

        let index = self.round_robin_counter.fetch_add(1, Ordering::Relaxed);
        let replicas = self.discovery.get_replicas().await;

        if let Some(replica) = replicas.get(index % replicas.len()) {
            if let Some(pool) = read_pools.get(&replica.host) {
                match pool.get().await {
                    Ok(conn) => return Ok(conn),
                    Err(e) => {
                        warn!(
                            "Failed to get connection from replica {}: {}",
                            replica.name, e
                        );
                    }
                }
            }
        }

        // Fallback to leader
        debug!("Round-robin selection failed, falling back to leader");
        self.get_write_connection().await
    }

    /// Get read connection from replica with least lag
    async fn get_read_connection_least_lag(&self) -> Result<PooledConnection> {
        let read_pools = self.read_pools.read().await;

        if read_pools.is_empty() {
            debug!("No read replicas available, falling back to leader");
            return self.get_write_connection().await;
        }

        // Get replica with least lag
        if let Some(replica) = self
            .discovery
            .get_least_lag_replica(self.max_replica_lag_ms)
            .await
        {
            if let Some(pool) = read_pools.get(&replica.host) {
                match pool.get().await {
                    Ok(conn) => {
                        debug!(
                            "Using replica {} with lag {:?}ms",
                            replica.name, replica.lag
                        );
                        return Ok(conn);
                    }
                    Err(e) => {
                        warn!(
                            "Failed to get connection from replica {}: {}",
                            replica.name, e
                        );
                    }
                }
            }
        }

        // Fallback to leader
        debug!("No suitable replica found, falling back to leader");
        self.get_write_connection().await
    }

    /// Handle leader change event
    pub async fn handle_leader_change(&self) -> Result<()> {
        warn!("Handling leader change");

        // Get new leader
        let leader = self
            .discovery
            .get_leader()
            .await
            .ok_or_else(|| anyhow!("No leader available after failover"))?;

        // Recreate write pool
        self.create_write_pool(&leader).await?;

        // Update read pools
        self.update_read_pools().await?;

        info!("Leader change handled successfully");
        Ok(())
    }

    /// Start background tasks for cluster management
    pub fn start_background_tasks(self: Arc<Self>) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(30));
            let mut last_leader: Option<String> = None;

            loop {
                interval.tick().await;

                // Check for leader changes
                if let Some(leader) = manager.discovery.get_leader().await {
                    let current_leader = Some(leader.host.clone());
                    if last_leader != current_leader {
                        if last_leader.is_some() {
                            // Leader changed
                            info!("Leader change detected");
                            if let Err(e) = manager.handle_leader_change().await {
                                error!("Failed to handle leader change: {}", e);
                            }
                        }
                        last_leader = current_leader;
                    }
                }

                // Update read pools periodically
                if let Err(e) = manager.update_read_pools().await {
                    error!("Failed to update read pools: {}", e);
                }
            }
        });
    }

    /// Get statistics about the cluster
    pub async fn get_stats(&self) -> ClusterStats {
        let write_available = self.write_pool.read().await.is_some();
        let read_pool_count = self.read_pools.read().await.len();
        let leader = self.discovery.get_leader().await;
        let replicas = self.discovery.get_replicas().await;

        ClusterStats {
            write_available,
            read_pool_count,
            leader_name: leader.map(|l| l.name),
            replica_count: replicas.len(),
            state_age_secs: self.discovery.get_state_age_secs().await,
        }
    }

    /// Get a clone of the write pool for direct access
    pub async fn get_write_pool(&self) -> Result<Pool> {
        let pool_guard = self.write_pool.read().await;
        pool_guard
            .as_ref()
            .ok_or_else(|| anyhow!("Write pool not initialized"))
            .cloned()
    }
}

#[derive(Debug)]
pub struct ClusterStats {
    pub write_available: bool,
    pub read_pool_count: usize,
    pub leader_name: Option<String>,
    pub replica_count: usize,
    pub state_age_secs: Option<u64>,
}
