// Regression test for the 2026-07-12 outage root cause: pool handles cloned at
// startup (as `Database::new` hands to every repository) must observe the pool
// the ClusterManager installs for a new leader. Previously the manager swapped
// a pool nothing re-read, so repositories kept dialing the host captured at
// startup until the process was redeployed.

use database::cluster_manager::{ClusterManager, DatabaseConfig, ReadPreference};
use database::patroni_discovery::{ClusterMember, PatroniDiscovery};
use std::sync::Arc;
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

/// Listener that accepts TCP connections and immediately closes them, standing
/// in for a failed leader that is still reachable at the TCP level.
async fn dead_postgres_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            if let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        }
    });
    port
}

#[tokio::test]
async fn reconcile_repoints_startup_pool_clones_to_new_leader() {
    let pg_host = std::env::var("DATABASE_HOST").unwrap_or_else(|_| "localhost".to_string());
    let pg_port: u16 = std::env::var("DATABASE_PORT")
        .unwrap_or_else(|_| "5432".to_string())
        .parse()
        .unwrap();

    // "Old leader": up at the TCP level but unusable, like a failed member.
    let dead_port = dead_postgres_port().await;
    // Refresh interval long enough that the injected state never goes stale.
    let discovery = Arc::new(PatroniDiscovery::new(
        "test-app".to_string(),
        "gateway.invalid".to_string(),
        3600,
    ));
    discovery
        .set_cluster_state_for_test(
            Some(leader_member("old-leader", "127.0.0.1", dead_port)),
            vec![],
        )
        .await;

    let manager = ClusterManager::new(
        discovery.clone(),
        DatabaseConfig {
            database: std::env::var("DATABASE_NAME").unwrap_or_else(|_| "postgres".to_string()),
            username: std::env::var("DATABASE_USERNAME").unwrap_or_else(|_| "postgres".to_string()),
            password: std::env::var("DATABASE_PASSWORD").unwrap_or_else(|_| "postgres".to_string()),
            max_write_connections: 2,
            max_read_connections: 2,
            tls_enabled: false,
            tls_ca_cert_path: None,
        },
        ReadPreference::LeaderOnly,
        None,
    );

    // What Database::new does at startup: take a handle and clone it into the
    // repositories.
    let repository_handle = manager.write_pool();

    manager.reconcile().await;
    assert!(
        repository_handle.get().await.is_err(),
        "no pool must be installed while the leader is unusable"
    );

    // Failover: discovery now reports a reachable leader.
    discovery
        .set_cluster_state_for_test(Some(leader_member("new-leader", &pg_host, pg_port)), vec![])
        .await;
    manager.reconcile().await;

    let conn = repository_handle
        .get()
        .await
        .expect("startup handle must serve connections to the new leader");
    let row = conn.query_one("SELECT 1", &[]).await.unwrap();
    assert_eq!(row.get::<_, i32>(0), 1);
}
