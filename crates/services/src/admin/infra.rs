//! Infrastructure / fleet burn summary.
//!
//! Fetches the live GPU-host inventory and current per-model GPU allocation from
//! internal observability endpoints (reachable only server-side, not from the
//! browser). Results are cached for a few minutes; unavailable sources use the
//! last-known value (or an empty fallback) and are explicitly marked stale.
//!
//! No customer data is involved — only host IPs, model identifiers, and counts.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use utoipa::ToSchema;

/// How long a fetched inventory stays fresh before we refetch.
const CACHE_TTL: Duration = Duration::from_secs(300);

/// One host and the models it is currently serving. Internal only — host
/// identities (IPs/model lists) are NEVER exposed in the API response.
#[derive(Debug, Clone)]
struct HostInfo {
    #[allow(dead_code)]
    host: String,
    models: Vec<String>,
}

/// Fleet burn summary returned by the admin endpoint.
///
/// Exposes only counts and burn — never host IPs or per-host model lists.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InfraSummary {
    pub total_hosts: i64,
    /// Hosts serving ≥1 model.
    pub active_hosts: i64,
    /// Hosts serving no models (idle capacity we still pay for).
    pub idle_hosts: i64,
    pub cost_per_host_usd_month: f64,
    pub monthly_burn_usd: f64,
    pub daily_burn_usd: f64,
    /// Planning rate for one allocated physical GPU-hour.
    pub cost_per_gpu_hour_usd: f64,
    /// Current distinct physical GPUs allocated to a model workload.
    pub total_allocated_gpus: i64,
    /// Current projected fleet GPU burn per hour.
    pub hourly_gpu_burn_usd: f64,
    /// Current allocation grouped by the model label emitted by DCGM.
    pub model_gpu_allocations: Vec<ModelGpuAllocation>,
    /// True when current GPU allocation is unavailable or served from cache.
    pub gpu_data_stale: bool,
    pub fetched_at: DateTime<Utc>,
    /// True when this is last-known / fallback data because the live fetch failed.
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ModelGpuAllocation {
    pub model_name: String,
    pub allocated_gpus: i64,
}

#[derive(Debug, Deserialize)]
struct PrometheusResponse {
    status: String,
    data: Option<PrometheusData>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PrometheusData {
    result: Vec<PrometheusSeries>,
}

#[derive(Debug, Deserialize)]
struct PrometheusSeries {
    metric: PrometheusMetric,
    value: (f64, String),
}

#[derive(Debug, Deserialize)]
struct PrometheusMetric {
    model: Option<String>,
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
    prometheus_url: Option<String>,
    prometheus_bearer_token: Option<String>,
    prometheus_environment: String,
    cost_per_gpu_hour_usd: f64,
    client: reqwest::Client,
    cache: RwLock<Option<Cached>>,
}

impl InfraService {
    pub fn new(
        machines_url: Option<String>,
        cost_per_host_usd_month: f64,
        prometheus_url: Option<String>,
        prometheus_bearer_token: Option<String>,
        prometheus_environment: String,
        cost_per_gpu_hour_usd: f64,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self {
            machines_url,
            cost_per_host_usd_month,
            prometheus_url,
            prometheus_bearer_token,
            prometheus_environment,
            cost_per_gpu_hour_usd,
            client,
            cache: RwLock::new(None),
        }
    }

    /// Return the fleet burn summary, using the cache when fresh.
    pub async fn get_infra_summary(&self) -> InfraSummary {
        // Fast path: fresh cache.
        if let Some(cached) = self.cache.read().await.as_ref() {
            if cached.at.elapsed() < CACHE_TTL {
                return cached.summary.clone();
            }
        }

        let (hosts, host_stale) = match self.machines_url.as_deref() {
            Some(url) => match self.fetch_machines(url).await {
                Ok(hosts) => (Some(hosts), false),
                Err(e) => {
                    tracing::warn!("infra inventory fetch failed: {e}");
                    (None, true)
                }
            },
            None => (None, true),
        };

        let (gpu_allocations, gpu_data_stale) = match self.prometheus_url.as_deref() {
            Some(url) => match self.fetch_gpu_allocations(url).await {
                Ok(allocations) => (Some(allocations), false),
                Err(e) => {
                    tracing::warn!("infra Prometheus fetch failed: {e}");
                    (None, true)
                }
            },
            None => (None, true),
        };

        let mut summary = self.summarize(
            hosts.unwrap_or_default(),
            gpu_allocations.unwrap_or_default(),
            host_stale,
            gpu_data_stale,
        );

        let mut guard = self.cache.write().await;
        if let Some(previous) = guard.as_ref().map(|cached| &cached.summary) {
            if host_stale {
                summary.total_hosts = previous.total_hosts;
                summary.active_hosts = previous.active_hosts;
                summary.idle_hosts = previous.idle_hosts;
                summary.monthly_burn_usd = previous.monthly_burn_usd;
                summary.daily_burn_usd = previous.daily_burn_usd;
                summary.fetched_at = previous.fetched_at;
            }
            if gpu_data_stale {
                summary.total_allocated_gpus = previous.total_allocated_gpus;
                summary.hourly_gpu_burn_usd = previous.hourly_gpu_burn_usd;
                summary.model_gpu_allocations = previous.model_gpu_allocations.clone();
            }
        }

        *guard = Some(Cached {
            summary: summary.clone(),
            at: Instant::now(),
        });
        summary
    }

    /// Build a summary from a parsed host list.
    fn summarize(
        &self,
        hosts: Vec<HostInfo>,
        model_gpu_allocations: Vec<ModelGpuAllocation>,
        stale: bool,
        gpu_data_stale: bool,
    ) -> InfraSummary {
        let total_hosts = hosts.len() as i64;
        let active_hosts = hosts.iter().filter(|h| !h.models.is_empty()).count() as i64;
        let idle_hosts = total_hosts - active_hosts;
        let monthly_burn_usd = total_hosts as f64 * self.cost_per_host_usd_month;
        let total_allocated_gpus = model_gpu_allocations
            .iter()
            .map(|allocation| allocation.allocated_gpus)
            .sum();
        InfraSummary {
            total_hosts,
            active_hosts,
            idle_hosts,
            cost_per_host_usd_month: self.cost_per_host_usd_month,
            monthly_burn_usd,
            daily_burn_usd: monthly_burn_usd / 30.4,
            cost_per_gpu_hour_usd: self.cost_per_gpu_hour_usd,
            total_allocated_gpus,
            hourly_gpu_burn_usd: total_allocated_gpus as f64 * self.cost_per_gpu_hour_usd,
            model_gpu_allocations,
            gpu_data_stale,
            fetched_at: Utc::now(),
            stale,
        }
    }

    async fn fetch_machines(&self, url: &str) -> Result<Vec<HostInfo>, String> {
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

    async fn fetch_gpu_allocations(&self, url: &str) -> Result<Vec<ModelGpuAllocation>, String> {
        let env = escape_promql_label_value(&self.prometheus_environment);
        let query = format!(
            "count by (model) (count by (model, host_machine, UUID) (DCGM_FI_DEV_GPU_UTIL{{env=\"{env}\"}}))"
        );
        let endpoint = format!("{}/api/v1/query", url.trim_end_matches('/'));
        let mut request = self.client.get(endpoint).query(&[("query", query)]);
        if let Some(token) = self.prometheus_bearer_token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("status {}", response.status()));
        }
        let body = response.text().await.map_err(|e| e.to_string())?;
        parse_prometheus_allocations(&body)
    }
}

fn escape_promql_label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

fn parse_prometheus_allocations(body: &str) -> Result<Vec<ModelGpuAllocation>, String> {
    let response: PrometheusResponse = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if response.status != "success" {
        return Err(response
            .error
            .unwrap_or_else(|| "Prometheus query failed".to_string()));
    }
    let data = response
        .data
        .ok_or_else(|| "Prometheus response missing data".to_string())?;
    let mut allocations = Vec::new();
    for series in data.result {
        let Some(model_name) = series.metric.model.filter(|value| !value.is_empty()) else {
            continue;
        };
        let value = series.value.1.parse::<f64>().map_err(|e| e.to_string())?;
        if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > i64::MAX as f64 {
            return Err(format!("invalid GPU count for model {model_name}"));
        }
        allocations.push(ModelGpuAllocation {
            model_name,
            allocated_gpus: value as i64,
        });
    }
    allocations.sort_by(|a, b| a.model_name.cmp(&b.model_name));
    Ok(allocations)
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
        let svc = InfraService::new(
            Some("http://unused".to_string()),
            1000.0,
            None,
            None,
            "prod".to_string(),
            2.0,
        );
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
        let allocations = vec![
            ModelGpuAllocation {
                model_name: "model-a".into(),
                allocated_gpus: 4,
            },
            ModelGpuAllocation {
                model_name: "shared".into(),
                allocated_gpus: 1,
            },
        ];
        let s = svc.summarize(hosts, allocations, false, false);
        assert_eq!(s.total_hosts, 2);
        assert_eq!(s.active_hosts, 1);
        assert_eq!(s.idle_hosts, 1);
        assert_eq!(s.monthly_burn_usd, 2000.0);
        assert_eq!(s.total_allocated_gpus, 5);
        assert_eq!(s.hourly_gpu_burn_usd, 10.0);
        assert!(!s.gpu_data_stale);
    }

    #[test]
    fn parses_prometheus_model_allocations() {
        let body = r#"{
            "status": "success",
            "data": {
                "resultType": "vector",
                "result": [
                    {"metric": {"model": "shared"}, "value": [1788380000.0, "1"]},
                    {"metric": {"model": "z-ai/glm-5.2"}, "value": [1788380000.0, "8"]}
                ]
            }
        }"#;
        let allocations = parse_prometheus_allocations(body).expect("valid Prometheus response");
        assert_eq!(
            allocations,
            vec![
                ModelGpuAllocation {
                    model_name: "shared".into(),
                    allocated_gpus: 1,
                },
                ModelGpuAllocation {
                    model_name: "z-ai/glm-5.2".into(),
                    allocated_gpus: 8,
                },
            ]
        );
    }

    #[test]
    fn rejects_fractional_gpu_counts() {
        let body = r#"{
            "status": "success",
            "data": {
                "resultType": "vector",
                "result": [
                    {"metric": {"model": "broken"}, "value": [1788380000.0, "1.5"]}
                ]
            }
        }"#;
        assert!(parse_prometheus_allocations(body).is_err());
    }
}
