use crate::patroni_discovery::{ClusterMember, PatroniDiscovery};
use crate::pool::{create_pool_with_native_tls, DbPool};
use anyhow::{anyhow, Result};
use deadpool::managed::QueueMode;
use deadpool_postgres::{Config, Object as PooledConnection, Pool, Runtime};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};
use tokio::time;
use tracing::{debug, error, info, warn};

/// Upper bound on verifying a candidate leader before installing its pool.
/// Generous next to the 5s pool create/wait timeouts it wraps, but bounded so
/// a member that accepts connections and then hangs cannot stall the
/// reconcile loop.
const WRITE_POOL_VERIFY_TIMEOUT: Duration = Duration::from_secs(15);

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
    /// Shared handle the repositories clone at startup. Installing a new pool
    /// here repoints every clone; see [`DbPool`].
    write_pool: DbPool,
    /// `host:port` the write pool currently targets. Recorded only after a
    /// pool was successfully built and verified against that member, so a
    /// failed rebuild leaves it unchanged and the next reconcile tick retries.
    write_pool_target: Arc<RwLock<Option<String>>>,
    /// Serializes reconciliation so concurrent callers cannot interleave
    /// verify/install sequences and roll the write pool back to an older
    /// leader (last-writer-wins).
    reconcile_lock: Mutex<()>,
    read_pools: Arc<RwLock<HashMap<String, Pool>>>,
    database_config: DatabaseConfig,
    read_preference: ReadPreference,
    max_replica_lag_ms: Option<i64>,
    round_robin_counter: AtomicUsize,
    background_task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
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
            write_pool: DbPool::uninitialized(),
            write_pool_target: Arc::new(RwLock::new(None)),
            reconcile_lock: Mutex::new(()),
            read_pools: Arc::new(RwLock::new(HashMap::new())),
            database_config,
            read_preference,
            max_replica_lag_ms,
            round_robin_counter: AtomicUsize::new(0),
            background_task_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// `host:port` connection target for a cluster member.
    fn member_target(member: &ClusterMember) -> String {
        format!("{}:{}", member.host, member.port)
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
        debug!(
            "Creating write pool for leader: {} ({}:{})",
            leader.name, leader.host, leader.port
        );

        let pool = self.create_pool(
            &leader.host,
            leader.port,
            self.database_config.max_write_connections,
        )?;

        // The deadpool timeouts only bound connection establishment; a member
        // that accepts connections but then hangs would otherwise block the
        // reconcile loop indefinitely and freeze all failover handling, so the
        // whole verification is bounded.
        let verification = async {
            // Test the connection
            let conn = pool.get().await?;
            conn.simple_query("SELECT 1").await?;

            // Verify this is actually the leader. Patroni can report a member
            // as leader while Postgres is still completing promotion;
            // installing it would pin every write to a read-only node with no
            // retry, so fail and let the next reconcile tick try again.
            let row = conn.query_one("SELECT pg_is_in_recovery()", &[]).await?;
            let is_replica: bool = row.try_get(0)?;
            if is_replica {
                return Err(anyhow!(
                    "Node {} claims to be leader but is still in recovery",
                    leader.name
                ));
            }
            Ok(())
        };
        time::timeout(WRITE_POOL_VERIFY_TIMEOUT, verification)
            .await
            .map_err(|_| {
                anyhow!(
                    "Timed out verifying leader {} after {:?}",
                    leader.name,
                    WRITE_POOL_VERIFY_TIMEOUT
                )
            })??;

        // Discovery may have advanced while this candidate was being verified
        // (up to WRITE_POOL_VERIFY_TIMEOUT). Installing a stale leader would
        // repoint every repository to a demoted node until the next reconcile
        // tick, so confirm the candidate is still the current leader.
        let target = Self::member_target(leader);
        let current = self.discovery.get_leader().await;
        if current.as_ref().map(Self::member_target).as_deref() != Some(target.as_str()) {
            return Err(anyhow!(
                "Cluster leader changed to {:?} while verifying {target}; discarding this pool",
                current.map(|m| Self::member_target(&m))
            ));
        }

        self.write_pool.replace(pool);
        *self.write_pool_target.write().await = Some(target.clone());

        info!("Write pool now targets leader {} ({})", leader.name, target);
        Ok(())
    }

    /// Update read pools based on current replicas
    async fn update_read_pools(&self) -> Result<()> {
        let replicas = self.discovery.get_replicas().await;
        debug!("Updating read pools for {} replicas", replicas.len());
        let mut read_pools = self.read_pools.write().await;

        // Remove pools for replicas that no longer exist
        read_pools.retain(|host, _| replicas.iter().any(|r| &r.host == host));

        // Add pools for new replicas
        for replica in replicas {
            if !read_pools.contains_key(&replica.host) {
                debug!(
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
                        debug!("Read pool created for replica {}", replica.name);
                    }
                    Err(e) => {
                        error!(
                            "Failed to create pool for replica {}: {:?}",
                            replica.name, e
                        );
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
        self.write_pool
            .get()
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
        let replicas = self.discovery.get_replicas().await;
        if replicas.is_empty() {
            debug!("No read replicas available, falling back to leader");
            return self.get_write_connection().await;
        }

        let index = self.round_robin_counter.fetch_add(1, Ordering::Relaxed);
        if let Some(replica) = replicas.get(index % replicas.len()) {
            // Clone the pool out of the map so the lock is not held across the
            // acquisition await.
            let pool = self.read_pools.read().await.get(&replica.host).cloned();
            if let Some(pool) = pool {
                match pool.get().await {
                    Ok(conn) => return Ok(conn),
                    Err(e) => {
                        warn!(
                            "Failed to get connection from replica {}: {:?}",
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
        // Get replica with least lag
        if let Some(replica) = self
            .discovery
            .get_least_lag_replica(self.max_replica_lag_ms)
            .await
        {
            // Clone the pool out of the map so the lock is not held across the
            // acquisition await.
            let pool = self.read_pools.read().await.get(&replica.host).cloned();
            if let Some(pool) = pool {
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
                            "Failed to get connection from replica {}: {:?}",
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

    /// Converge the pools on the current cluster topology: rebuild the write
    /// pool whenever it does not target the discovered leader, and refresh the
    /// read pools. Safe to call repeatedly — the installed target is only
    /// recorded on a successful rebuild, so a failure (e.g. the new leader not
    /// accepting connections yet) is retried on the next tick instead of being
    /// dropped.
    pub async fn reconcile(&self) {
        // One reconciliation at a time: interleaved verify/install sequences
        // could install pools out of order and roll back to an older leader.
        let _guard = self.reconcile_lock.lock().await;

        if self.discovery.is_state_stale().await {
            let age = self
                .discovery
                .get_state_age_secs()
                .await
                .map(|secs| format!("{secs}s"))
                .unwrap_or_else(|| "never loaded".to_string());
            warn!("Patroni cluster state is stale (age: {age}); leader changes may go undetected");
        }

        match self.discovery.get_leader().await {
            Some(leader) => {
                let target = Self::member_target(&leader);
                let installed = self.write_pool_target.read().await.clone();
                if installed.as_deref() != Some(target.as_str()) {
                    warn!(
                        "Write pool targets {installed:?} but cluster leader is {target}; rebuilding write pool"
                    );
                    if let Err(e) = self.create_write_pool(&leader).await {
                        error!(
                            "Failed to rebuild write pool for leader {target} (will retry): {e:#}"
                        );
                    }
                }
            }
            None => {
                warn!("No cluster leader known; keeping current write pool");
            }
        }

        if let Err(e) = self.update_read_pools().await {
            error!("Failed to update read pools: {e:#}");
        }
    }

    /// Start background tasks for cluster management and store handle for lifecycle management
    pub async fn start_background_tasks(self: Arc<Self>) {
        let handle = tokio::spawn({
            let manager = self.clone();
            async move {
                let mut interval = time::interval(Duration::from_secs(30));
                interval.tick().await; // skip the immediate first tick

                loop {
                    interval.tick().await;
                    manager.reconcile().await;
                }
            }
        });

        let mut task_handle = self.background_task_handle.lock().await;
        if let Some(previous) = task_handle.replace(handle) {
            previous.abort();
        }
    }

    /// Get statistics about the cluster
    pub async fn get_stats(&self) -> ClusterStats {
        let write_available = self.write_pool.current().is_some();
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

    /// Get the shared write-pool handle. Clones of this handle stay pointed at
    /// the current leader across failovers.
    pub fn write_pool(&self) -> DbPool {
        self.write_pool.clone()
    }

    /// Shutdown the cluster manager and cancel background tasks
    pub async fn shutdown(&self) {
        info!("Shutting down cluster manager");

        let mut task_handle = self.background_task_handle.lock().await;
        if let Some(handle) = task_handle.take() {
            debug!("Cancelling background monitoring task");
            handle.abort();
            info!("Background monitoring task cancelled successfully");
        } else {
            debug!("No active background task to cancel");
        }

        // Shutdown the discovery service as well
        self.discovery.shutdown().await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn leader_member(name: &str, host: &str, port: u16) -> ClusterMember {
        ClusterMember {
            name: name.to_string(),
            host: host.to_string(),
            port,
            role: "leader".to_string(),
            state: "running".to_string(),
            lag: None,
            timeline: None,
        }
    }

    fn test_db_config() -> DatabaseConfig {
        DatabaseConfig {
            database: std::env::var("DATABASE_NAME").unwrap_or_else(|_| "postgres".to_string()),
            username: std::env::var("DATABASE_USERNAME").unwrap_or_else(|_| "postgres".to_string()),
            password: std::env::var("DATABASE_PASSWORD").unwrap_or_else(|_| "postgres".to_string()),
            max_write_connections: 2,
            max_read_connections: 2,
            tls_enabled: false,
            tls_ca_cert_path: None,
        }
    }

    /// A discovery whose refresh interval is long enough that the injected
    /// state never counts as stale during a test.
    fn test_discovery() -> Arc<PatroniDiscovery> {
        Arc::new(PatroniDiscovery::new(
            "test-app".to_string(),
            "gateway.invalid".to_string(),
            3600,
        ))
    }

    /// Listener that accepts TCP connections and immediately closes them, so
    /// the Postgres handshake fails. Returns the port and an attempt counter.
    async fn dead_postgres() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        tokio::spawn(async move {
            loop {
                if let Ok((socket, _)) = listener.accept().await {
                    counter.fetch_add(1, Ordering::SeqCst);
                    drop(socket);
                }
            }
        });
        (port, attempts)
    }

    /// Regression test for the 2026-07-12 outage follow-up: a failed write-pool
    /// rebuild (new leader not accepting connections yet) must be retried on
    /// the next reconcile tick, not dropped. The old loop recorded the new
    /// leader as handled even when the rebuild failed, wedging the write pool
    /// on the previous leader until a redeploy.
    #[tokio::test]
    async fn failed_write_pool_rebuild_is_retried_on_next_reconcile() {
        let (port, attempts) = dead_postgres().await;
        let discovery = test_discovery();
        discovery
            .set_cluster_state_for_test(Some(leader_member("n1", "127.0.0.1", port)), vec![])
            .await;

        let manager = ClusterManager::new(
            discovery.clone(),
            test_db_config(),
            ReadPreference::LeaderOnly,
            None,
        );

        manager.reconcile().await;
        let first_attempts = attempts.load(Ordering::SeqCst);
        assert!(first_attempts >= 1, "reconcile must attempt a connection");
        assert!(
            manager.write_pool_target.read().await.is_none(),
            "a failed rebuild must not record the leader as installed"
        );

        manager.reconcile().await;
        assert!(
            attempts.load(Ordering::SeqCst) > first_attempts,
            "the next reconcile must retry the failed rebuild"
        );
    }

    // The success-path regression test (a startup pool handle following a
    // leader change to a live Postgres) needs a real database and lives in the
    // e2e suite: crates/api/tests/e2e_all/patroni_failover.rs.
}
