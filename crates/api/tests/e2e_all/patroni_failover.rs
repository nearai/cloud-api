// Regression tests for the 2026-07-12 outage root cause: pool handles cloned
// at startup (as `Database::new` hands to every repository) must observe the
// pool the ClusterManager installs for a new leader. Previously the manager
// swapped a pool nothing re-read, so repositories kept dialing the host
// captured at startup until the process was redeployed.
//
// Both "leaders" are TCP proxies in front of the CI Postgres, so the tests can
// tell which path a connection takes and can kill the old leader's path
// outright.

use database::cluster_manager::{ClusterManager, DatabaseConfig, ReadPreference};
use database::patroni_discovery::{ClusterMember, PatroniDiscovery};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

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

/// Discovery whose refresh interval is long enough that injected state never
/// counts as stale during a test.
fn test_discovery() -> Arc<PatroniDiscovery> {
    Arc::new(PatroniDiscovery::new(
        "test-app".to_string(),
        "gateway.invalid".to_string(),
        3600,
    ))
}

fn postgres_upstream() -> String {
    let host = std::env::var("DATABASE_HOST").unwrap_or_else(|_| "localhost".to_string());
    let port = std::env::var("DATABASE_PORT").unwrap_or_else(|_| "5432".to_string());
    format!("{host}:{port}")
}

/// TCP proxy in front of Postgres standing in for one cluster member. Counts
/// accepted connections, optionally delays each connection before it reaches
/// Postgres (to widen verification windows), and can be shut down hard —
/// killing established connections — like a leader going down.
struct TcpProxy {
    port: u16,
    connections: Arc<AtomicUsize>,
    tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl TcpProxy {
    async fn start(upstream: String, pre_connect_delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(AtomicUsize::new(0));
        let tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::default();

        let accept_task = {
            let connections = connections.clone();
            let tasks = tasks.clone();
            tokio::spawn(async move {
                loop {
                    if let Ok((mut client, _)) = listener.accept().await {
                        connections.fetch_add(1, Ordering::SeqCst);
                        let upstream = upstream.clone();
                        let conn_task = tokio::spawn(async move {
                            tokio::time::sleep(pre_connect_delay).await;
                            if let Ok(mut server) = TcpStream::connect(&upstream).await {
                                let _ =
                                    tokio::io::copy_bidirectional(&mut client, &mut server).await;
                            }
                        });
                        tasks.lock().unwrap().push(conn_task);
                    }
                }
            })
        };
        tasks.lock().unwrap().push(accept_task);

        Self {
            port,
            connections,
            tasks,
        }
    }

    fn target(&self) -> ClusterMember {
        leader_member(&format!("member-{}", self.port), "127.0.0.1", self.port)
    }

    /// Stop accepting and kill every established connection — the member is
    /// hard-down.
    fn shutdown(&self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

/// The outage invariant: a handle clone already serving traffic through the
/// old leader's pool must observe the failover install and route subsequent
/// acquisitions through the new leader — even once the old leader is gone.
#[tokio::test]
async fn startup_pool_clones_follow_leader_across_failover() {
    let upstream = postgres_upstream();
    let leader_a = TcpProxy::start(upstream.clone(), Duration::ZERO).await;
    let leader_b = TcpProxy::start(upstream, Duration::ZERO).await;

    let discovery = test_discovery();
    discovery
        .set_cluster_state_for_test(Some(leader_a.target()), vec![])
        .await;

    let manager = ClusterManager::new(
        discovery.clone(),
        test_db_config(),
        ReadPreference::LeaderOnly,
        None,
    );
    manager.reconcile().await;

    // What Database::new does at startup: clone the handle into repositories.
    let repository_handle = manager.write_pool();
    let conn = repository_handle
        .get()
        .await
        .expect("must serve through leader A before the failover");
    let row = conn.query_one("SELECT 1", &[]).await.unwrap();
    assert_eq!(row.get::<_, i32>(0), 1);
    drop(conn);
    assert!(
        leader_a.connections.load(Ordering::SeqCst) >= 1,
        "pre-failover traffic must flow through leader A"
    );

    // Failover: discovery reports B as leader; reconcile installs it, then A
    // goes hard-down. With the outage bug, the clone stayed pinned to A and
    // wedged right here.
    discovery
        .set_cluster_state_for_test(Some(leader_b.target()), vec![])
        .await;
    manager.reconcile().await;
    leader_a.shutdown();

    let conn = repository_handle
        .get()
        .await
        .expect("startup clone must route through leader B after the failover");
    let row = conn.query_one("SELECT 1", &[]).await.unwrap();
    assert_eq!(row.get::<_, i32>(0), 1);
    assert!(
        leader_b.connections.load(Ordering::SeqCst) >= 1,
        "post-failover traffic must flow through leader B"
    );
}

/// If discovery advances to a new leader while a candidate is still being
/// verified, the stale candidate must be discarded, not installed.
#[tokio::test]
async fn stale_candidate_is_discarded_when_leader_changes_during_verification() {
    let upstream = postgres_upstream();
    // Candidate A delays every connection, holding reconcile inside
    // verification long enough to flip the leader underneath it.
    let leader_a = TcpProxy::start(upstream.clone(), Duration::from_millis(750)).await;
    let leader_b = TcpProxy::start(upstream, Duration::ZERO).await;

    let discovery = test_discovery();
    discovery
        .set_cluster_state_for_test(Some(leader_a.target()), vec![])
        .await;

    let manager = Arc::new(ClusterManager::new(
        discovery.clone(),
        test_db_config(),
        ReadPreference::LeaderOnly,
        None,
    ));
    let repository_handle = manager.write_pool();

    let reconcile_task = tokio::spawn({
        let manager = manager.clone();
        async move { manager.reconcile().await }
    });
    // Wait until reconcile is inside A's delayed verification, then fail over.
    tokio::time::sleep(Duration::from_millis(250)).await;
    discovery
        .set_cluster_state_for_test(Some(leader_b.target()), vec![])
        .await;
    reconcile_task.await.unwrap();

    assert!(
        repository_handle.get().await.is_err(),
        "the stale candidate must not be installed as the write pool"
    );

    // The next tick converges on the current leader.
    manager.reconcile().await;
    let conn = repository_handle
        .get()
        .await
        .expect("reconcile must install the current leader");
    let row = conn.query_one("SELECT 1", &[]).await.unwrap();
    assert_eq!(row.get::<_, i32>(0), 1);
    assert!(leader_b.connections.load(Ordering::SeqCst) >= 1);
}
