use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};
use tokio::time;
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterMember {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub role: String,
    pub state: String,
    #[serde(default)]
    pub lag: Option<i64>,
    #[serde(default)]
    pub timeline: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterInfo {
    pub members: Vec<ClusterMember>,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClusterState {
    pub leader: Option<ClusterMember>,
    pub replicas: Vec<ClusterMember>,
    pub last_updated: std::time::Instant,
}

pub struct PatroniDiscovery {
    client: Client,
    postgres_app_id: String,
    gateway_subdomain: String,
    cluster_state: Arc<RwLock<Option<ClusterState>>>,
    refresh_interval: Duration,
    refresh_task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl PatroniDiscovery {
    pub fn new(
        postgres_app_id: String,
        gateway_subdomain: String,
        refresh_interval_secs: u64,
    ) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
            postgres_app_id,
            gateway_subdomain,
            cluster_state: Arc::new(RwLock::new(None)),
            refresh_interval: Duration::from_secs(refresh_interval_secs),
            refresh_task_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// Discover cluster topology via Patroni REST API through gateway
    pub async fn discover_cluster(&self) -> Result<ClusterInfo> {
        info!(
            "Discovering cluster via Patroni node: {}",
            self.postgres_app_id
        );

        let url = format!(
            "https://{}-8008.{}/cluster",
            self.postgres_app_id, self.gateway_subdomain
        );
        // Allow self-signed certificates
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to connect to Patroni API: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "No body".to_string());
            error!("Failed to get cluster info: {} - {}", status, body);
            return Err(anyhow!("HTTP {status}: {body}"));
        }

        let cluster_info: ClusterInfo = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse cluster info: {e}"))?;

        info!("Discovered {} nodes in cluster", cluster_info.members.len());

        for member in &cluster_info.members {
            debug!(
                "  - {} ({}): role={}, state={}, lag={:?}",
                member.name, member.host, member.role, member.state, member.lag
            );
        }

        Ok(cluster_info)
    }

    /// Update internal cluster state from discovery
    pub async fn update_cluster_state(&self) -> Result<()> {
        let cluster_info = self.discover_cluster().await?;

        let mut leader = None;
        let mut replicas = Vec::new();

        // Accept both "running" and "streaming" states as valid for member selection,
        // since Patroni nodes may report "streaming" when actively replicating data,
        // and both states indicate a healthy, participating cluster member.
        for member in cluster_info.members {
            if member.state == "running" || member.state == "streaming" {
                match member.role.as_str() {
                    "leader" | "master" => {
                        leader = Some(member);
                    }
                    "replica" | "sync_standby" | "async_standby" => {
                        replicas.push(member);
                    }
                    _ => {
                        debug!("Unknown role: {} for member {}", member.role, member.name);
                    }
                }
            }
        }

        let new_state = ClusterState {
            leader: leader.clone(),
            replicas: replicas.clone(),
            last_updated: std::time::Instant::now(),
        };

        // Check if leader changed
        let mut state_lock = self.cluster_state.write().await;
        let leader_changed = match &*state_lock {
            Some(old_state) => {
                let old_leader_host = old_state.leader.as_ref().map(|l| &l.host);
                let new_leader_host = leader.as_ref().map(|l| &l.host);
                old_leader_host != new_leader_host
            }
            None => true,
        };

        if leader_changed {
            if let Some(ref leader) = leader {
                debug!("Leader changed to: {} ({})", leader.name, leader.host);
            } else {
                warn!("No leader found in cluster!");
            }
        }

        info!(
            "Cluster state: leader={}, replicas={}",
            leader.is_some(),
            replicas.len()
        );

        *state_lock = Some(new_state);
        Ok(())
    }

    /// Get current leader information
    pub async fn get_leader(&self) -> Option<ClusterMember> {
        let state = self.cluster_state.read().await;
        state.as_ref()?.leader.clone()
    }

    /// Get all replica information
    pub async fn get_replicas(&self) -> Vec<ClusterMember> {
        let state = self.cluster_state.read().await;
        state
            .as_ref()
            .map(|s| s.replicas.clone())
            .unwrap_or_default()
    }

    /// Get replicas sorted by lag (lowest lag first)
    pub async fn get_replicas_by_lag(&self) -> Vec<ClusterMember> {
        let mut replicas = self.get_replicas().await;
        replicas.sort_by_key(|r| r.lag.unwrap_or(i64::MAX));
        replicas
    }

    /// Start background refresh task and store handle for lifecycle management
    pub async fn start_refresh_task(self: Arc<Self>) {
        let handle = tokio::spawn({
            let discovery = self.clone();
            async move {
                let mut interval = time::interval(discovery.refresh_interval);
                interval.tick().await; // Skip first immediate tick

                loop {
                    interval.tick().await;

                    match discovery.update_cluster_state().await {
                        Ok(_) => {
                            debug!("Cluster state refreshed successfully");
                        }
                        Err(e) => {
                            error!("Failed to refresh cluster state: {:?}", e);
                        }
                    }
                }
            }
        });

        let mut task_handle = self.refresh_task_handle.lock().await;
        *task_handle = Some(handle);
    }

    /// Get cluster state age in seconds
    pub async fn get_state_age_secs(&self) -> Option<u64> {
        let state = self.cluster_state.read().await;
        state.as_ref().map(|s| s.last_updated.elapsed().as_secs())
    }

    /// Check if cluster state is stale (older than 2x refresh interval)
    pub async fn is_state_stale(&self) -> bool {
        match self.get_state_age_secs().await {
            Some(age) => age > (self.refresh_interval.as_secs() * 2),
            None => true,
        }
    }

    /// Check if leader is available
    pub async fn has_leader(&self) -> bool {
        self.get_leader().await.is_some()
    }

    /// Get a read replica using round-robin selection
    pub async fn get_read_replica_round_robin(&self, index: usize) -> Option<ClusterMember> {
        let replicas = self.get_replicas().await;
        if replicas.is_empty() {
            return None;
        }
        Some(replicas[index % replicas.len()].clone())
    }

    /// Get the replica with the least lag
    pub async fn get_least_lag_replica(&self, max_lag_ms: Option<i64>) -> Option<ClusterMember> {
        let replicas = self.get_replicas_by_lag().await;
        for replica in replicas {
            if let Some(max_lag) = max_lag_ms {
                if let Some(lag) = replica.lag {
                    if lag <= max_lag {
                        return Some(replica);
                    }
                }
            } else {
                return Some(replica);
            }
        }
        None
    }

    /// Shutdown the cluster discovery and cancel the refresh task
    pub async fn shutdown(&self) {
        info!("Shutting down cluster discovery service");

        let mut task_handle = self.refresh_task_handle.lock().await;
        if let Some(handle) = task_handle.take() {
            debug!("Cancelling cluster refresh task");
            handle.abort();
            info!("Cluster refresh task cancelled successfully");
        } else {
            debug!("No active refresh task to cancel");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cluster_member_deserialization() {
        let json = r#"{
            "name": "postgres-1",
            "host": "postgres-1.example.com",
            "port": 5432,
            "role": "leader",
            "state": "running"
        }"#;

        let member: ClusterMember = serde_json::from_str(json).unwrap();
        assert_eq!(member.name, "postgres-1");
        assert_eq!(member.role, "leader");
        assert_eq!(member.state, "running");
        assert!(member.lag.is_none());
    }

    #[test]
    fn test_cluster_info_deserialization() {
        let json = r#"{
            "members": [
                {
                    "name": "postgres-1",
                    "host": "postgres-1.example.com",
                    "port": 5432,
                    "role": "leader",
                    "state": "running"
                },
                {
                    "name": "postgres-2",
                    "host": "postgres-2.example.com",
                    "port": 5432,
                    "role": "replica",
                    "state": "running",
                    "lag": 1024
                }
            ],
            "scope": "pg-cluster"
        }"#;

        let info: ClusterInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.members.len(), 2);
        assert_eq!(info.scope.as_deref(), Some("pg-cluster"));
    }
}
