//! Infrastructure / fleet burn summary.
//!
//! Fetches the live GPU-host inventory from an internal endpoint (reachable only
//! server-side, not from the browser), counts active vs idle hosts, and derives the
//! monthly/daily burn rate from a configurable cost-per-host. Results are cached for a
//! few minutes; if the inventory is unreachable we return the last-known value (or a
//! zero fallback) flagged `stale = true` so the dashboard never hard-fails.
//!
//! No customer data is involved — only host IPs, model identifiers, and counts.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// How long a fetched inventory stays fresh before we refetch.
const CACHE_TTL: Duration = Duration::from_secs(300);

/// One host and the models it is currently serving.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub host: String,
    pub models: Vec<String>,
}

/// Fleet burn summary returned by the admin endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfraSummary {
    pub total_hosts: i64,
    /// Hosts serving ≥1 model.
    pub active_hosts: i64,
    /// Hosts serving no models (idle capacity we still pay for).
    pub idle_hosts: i64,
    pub cost_per_host_usd_month: f64,
    pub monthly_burn_usd: f64,
    pub daily_burn_usd: f64,
    pub host_models: Vec<HostInfo>,
    pub fetched_at: DateTime<Utc>,
    /// True when this is last-known / fallback data because the live fetch failed.
    pub stale: bool,
}

struct Cached {
    summary: InfraSummary,
    at: Instant,
}

/// Service that fetches and caches the GPU-host inventory.
pub struct InfraService {
    /// Internal host-inventory endpoint. `None` disables the live fetch.
    machines_url: Option<String>,
    cost_per_host_usd_month: f64,
    client: reqwest::Client,
    cache: RwLock<Option<Cached>>,
}

impl InfraService {
    pub fn new(machines_url: Option<String>, cost_per_host_usd_month: f64) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self {
            machines_url,
            cost_per_host_usd_month,
            client,
            cache: RwLock::new(None),
        }
    }

    /// Return the fleet burn summary, using the cache when fresh.
    pub async fn get_infra_summary(&self) -> InfraSummary {
        // Not configured (no inventory endpoint): report no fleet data.
        let Some(url) = self.machines_url.clone() else {
            return self.summarize(Vec::new(), true);
        };

        // Fast path: fresh cache.
        if let Some(cached) = self.cache.read().await.as_ref() {
            if cached.at.elapsed() < CACHE_TTL {
                return cached.summary.clone();
            }
        }

        match self.fetch(&url).await {
            Ok(hosts) => {
                let summary = self.summarize(hosts, false);
                let mut guard = self.cache.write().await;
                *guard = Some(Cached {
                    summary: summary.clone(),
                    at: Instant::now(),
                });
                summary
            }
            Err(e) => {
                tracing::warn!("infra inventory fetch failed: {e}");
                // Serve last-known data marked stale, else a zero fallback.
                if let Some(cached) = self.cache.read().await.as_ref() {
                    let mut summary = cached.summary.clone();
                    summary.stale = true;
                    return summary;
                }
                self.summarize(Vec::new(), true)
            }
        }
    }

    /// Build a summary from a parsed host list.
    fn summarize(&self, hosts: Vec<HostInfo>, stale: bool) -> InfraSummary {
        let total_hosts = hosts.len() as i64;
        let active_hosts = hosts.iter().filter(|h| !h.models.is_empty()).count() as i64;
        let idle_hosts = total_hosts - active_hosts;
        let monthly_burn_usd = total_hosts as f64 * self.cost_per_host_usd_month;
        InfraSummary {
            total_hosts,
            active_hosts,
            idle_hosts,
            cost_per_host_usd_month: self.cost_per_host_usd_month,
            monthly_burn_usd,
            daily_burn_usd: monthly_burn_usd / 30.4,
            host_models: hosts,
            fetched_at: Utc::now(),
            stale,
        }
    }

    async fn fetch(&self, url: &str) -> Result<Vec<HostInfo>, String> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("status {}", resp.status()));
        }
        let body = resp.text().await.map_err(|e| e.to_string())?;
        Ok(parse_machines(&body))
    }
}

/// Parse the YAML-ish machines listing into hosts.
///
/// Format: a host line at column 0 ending with `:`, followed by indented `- port:model`
/// entries; `- (no models)` marks an idle host.
fn parse_machines(body: &str) -> Vec<HostInfo> {
    let mut hosts: Vec<HostInfo> = Vec::new();
    for raw in body.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        let indented = raw.starts_with(char::is_whitespace);
        let line = raw.trim();
        if !indented && line.ends_with(':') {
            // New host entry.
            let host = line.trim_end_matches(':').trim().to_string();
            hosts.push(HostInfo {
                host,
                models: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix('-') {
            let value = rest.trim();
            if value.is_empty() || value.eq_ignore_ascii_case("(no models)") {
                continue; // idle host: leave models empty
            }
            // Entry is "port:model"; keep the model identifier (after the first ':').
            let model = match value.split_once(':') {
                Some((_, model)) => model.trim().to_string(),
                None => value.to_string(),
            };
            if let Some(current) = hosts.last_mut() {
                current.models.push(model);
            }
        }
    }
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts_and_idle() {
        let body = "160.72.54.150:\n   - 8000:zai-org/GLM-5.1-FP8\n160.72.54.186:\n   - 8000:Qwen/Qwen3-30B\n   - 8001:openai/gpt-oss-120b\n160.72.54.207:\n   - (no models)\n";
        let hosts = parse_machines(body);
        assert_eq!(hosts.len(), 3);
        assert_eq!(hosts[0].host, "160.72.54.150");
        assert_eq!(hosts[0].models, vec!["zai-org/GLM-5.1-FP8"]);
        assert_eq!(hosts[1].models.len(), 2);
        assert!(hosts[2].models.is_empty()); // idle
    }

    #[test]
    fn summarize_counts_and_burn() {
        // Arbitrary non-real cost for the math check only.
        let svc = InfraService::new(Some("http://unused".to_string()), 1000.0);
        let hosts = vec![
            HostInfo {
                host: "a".into(),
                models: vec!["m".into()],
            },
            HostInfo {
                host: "b".into(),
                models: vec![],
            },
        ];
        let s = svc.summarize(hosts, false);
        assert_eq!(s.total_hosts, 2);
        assert_eq!(s.active_hosts, 1);
        assert_eq!(s.idle_hosts, 1);
        assert_eq!(s.monthly_burn_usd, 2000.0);
    }
}
