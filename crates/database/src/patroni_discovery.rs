use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Deserializer, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};
use tokio::time;
use tracing::{debug, error, info, warn};

/// Patroni reports `lag` as an integer for streaming members, but emits the
/// string `"unknown"` for members it cannot measure (e.g. `stopped`, `crashed`,
/// or `start failed` replicas). A bare `Option<i64>` rejects that string and
/// fails the entire `/cluster` parse, which would freeze topology discovery (or
/// abort startup) over a single unhealthy member. Coerce any non-integer value
/// to `None` so the rest of the cluster still parses.
fn deserialize_lag<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Lag {
        Int(i64),
        Other(serde::de::IgnoredAny),
    }

    Ok(match Option::<Lag>::deserialize(deserializer)? {
        Some(Lag::Int(n)) => Some(n),
        // "unknown", null, or any other non-integer value -> unknown lag
        _ => None,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterMember {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub role: String,
    pub state: String,
    #[serde(default, deserialize_with = "deserialize_lag")]
    pub lag: Option<i64>,
    #[serde(default)]
    pub timeline: Option<i64>,
}

/// Patroni lists a node as a member as soon as it registers in the DCS — which
/// happens *before* it publishes its `conn_url`. So a replica mid-creation (or
/// an uninitialized node) appears without `host`/`port`/`api_url`. Those fields
/// are required on `ClusterMember`, so a strict `Vec<ClusterMember>` parse fails
/// the ENTIRE `/cluster` response with `missing field \`host\`` over one
/// half-registered member — freezing topology discovery for every consumer
/// (cloud-api + chat-api) whenever a new postgres instance is being added.
///
/// Parse each member independently and drop any that don't fully deserialize; a
/// member with no connection info is not a usable leader/replica target anyway,
/// and the next refresh picks it up once it finishes registering. Same
/// resilience philosophy as `deserialize_lag` — one bad member must not poison
/// the whole cluster view.
fn deserialize_members<'de, D>(deserializer: D) -> Result<Vec<ClusterMember>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    let mut members = Vec::with_capacity(raw.len());
    for value in raw {
        match serde_json::from_value::<ClusterMember>(value.clone()) {
            Ok(member) => members.push(member),
            Err(e) => {
                let name = value
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<unknown>");
                warn!("Skipping not-yet-ready cluster member {name}: {e}");
            }
        }
    }
    Ok(members)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterInfo {
    #[serde(deserialize_with = "deserialize_members")]
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

    /// Run `f` with the current leader while holding the cluster-state read
    /// lock. Topology publication (`update_cluster_state`) takes the write
    /// lock, so it is excluded for the duration — letting callers make a
    /// leader comparison and a pool install atomic relative to discovery
    /// updates. `f` must be synchronous and quick; it runs under the lock.
    pub async fn with_current_leader<R>(&self, f: impl FnOnce(Option<&ClusterMember>) -> R) -> R {
        let state = self.cluster_state.read().await;
        f(state.as_ref().and_then(|s| s.leader.as_ref()))
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

    /// Test-only: inject a cluster state directly, bypassing HTTP discovery.
    /// Compiled only for this crate's tests and for dependents that enable the
    /// `test-support` feature (the e2e suite), so it does not ship in release
    /// builds.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_cluster_state_for_test(
        &self,
        leader: Option<ClusterMember>,
        replicas: Vec<ClusterMember>,
    ) {
        let mut state = self.cluster_state.write().await;
        *state = Some(ClusterState {
            leader,
            replicas,
            last_updated: std::time::Instant::now(),
        });
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

    #[test]
    fn test_member_with_string_lag_unknown() {
        // Patroni reports `lag` as the string "unknown" for stopped/crashed
        // members. This must not fail parsing of the member.
        let json = r#"{
            "name": "b5eecc86101d",
            "host": "postgres-prod-vzeis375.dstack.internal",
            "port": 5432,
            "role": "replica",
            "state": "stopped",
            "lag": "unknown"
        }"#;

        let member: ClusterMember = serde_json::from_str(json).unwrap();
        assert_eq!(member.state, "stopped");
        assert!(member.lag.is_none());
    }

    #[test]
    fn test_initializing_member_does_not_poison_parse() {
        // Regression: a replica mid-creation is registered in the DCS before it
        // publishes its conn_url, so Patroni omits host/port/api_url. Previously
        // this failed the whole parse with `missing field \`host\``, breaking
        // discovery for cloud-api + chat-api whenever a postgres instance was
        // added. The not-yet-ready member must be dropped, not poison the rest.
        let json = r#"{"members": [
          {"name":"leader1","role":"leader","state":"running","host":"postgres-a.dstack.internal","port":5432,"timeline":3},
          {"name":"newrep","role":"replica","state":"creating replica","timeline":3}
        ],"scope":"pg-cluster"}"#;
        let info: ClusterInfo =
            serde_json::from_str(json).expect("must parse despite half-registered member");
        assert_eq!(
            info.members.len(),
            1,
            "the not-yet-ready member should be dropped"
        );
        assert_eq!(info.members[0].name, "leader1");
        assert_eq!(info.members[0].role, "leader");
    }

    #[test]
    fn test_full_cluster_with_initializing_replica_parses() {
        // Real staging /cluster shape with an extra member mid-creation appended
        // (no host/port/api_url). The 4 ready members must still parse and the
        // leader must still be discoverable.
        let json = r#"{"members": [
          {"name":"a","role":"replica","state":"streaming","api_url":"http://[postgres-staging-5hbt5t4n.dstack.internal:8008]:8008/patroni","host":"postgres-staging-5hbt5t4n.dstack.internal","port":5432,"timeline":3,"lag":0},
          {"name":"b","role":"replica","state":"streaming","host":"postgres-yr6k7rmo.dstack.internal","port":5432,"timeline":3,"lag":0},
          {"name":"newrep","role":"replica","state":"creating replica","timeline":3},
          {"name":"leader","role":"leader","state":"running","host":"postgres-ew3zj5pk.dstack.internal","port":5432,"timeline":3}
        ],"scope":"pg-cluster"}"#;
        let info: ClusterInfo = serde_json::from_str(json).expect("must parse");
        assert_eq!(
            info.members.len(),
            3,
            "only the 3 fully-registered members survive"
        );
        assert!(info.members.iter().any(|m| m.role == "leader"));
        assert!(info.members.iter().all(|m| m.name != "newrep"));
    }

    #[test]
    fn test_cluster_with_stopped_member_string_lag() {
        // Regression: a single stopped replica reporting `"lag": "unknown"` must
        // not poison the whole cluster parse. Exact payload that previously
        // returned `invalid type: string "unknown", expected i64`.
        let json = r#"{"members": [{"name": "0513d70cb4dc", "role": "replica", "state": "streaming", "api_url": "http://[postgres-ikupakqr.dstack.internal:8008]:8008/patroni", "host": "postgres-ikupakqr.dstack.internal", "port": 5432, "timeline": 3, "lag": 0}, {"name": "b5eecc86101d", "role": "replica", "state": "stopped", "api_url": "http://[postgres-prod-vzeis375.dstack.internal:8008]:8008/patroni", "host": "postgres-prod-vzeis375.dstack.internal", "port": 5432, "lag": "unknown"}, {"name": "d2fb312c9ab6", "role": "leader", "state": "running", "api_url": "http://[postgres-qr5ygiq4.dstack.internal:8008]:8008/patroni", "host": "postgres-qr5ygiq4.dstack.internal", "port": 5432, "timeline": 3}], "scope": "pg-cluster"}"#;

        let info: ClusterInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.members.len(), 3);
        let stopped = info
            .members
            .iter()
            .find(|m| m.name == "b5eecc86101d")
            .unwrap();
        assert!(stopped.lag.is_none());
        let streaming = info
            .members
            .iter()
            .find(|m| m.name == "0513d70cb4dc")
            .unwrap();
        assert_eq!(streaming.lag, Some(0));
    }
}
