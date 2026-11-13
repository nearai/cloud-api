//! Analytics service for enterprise dashboard queries.
//!
//! This service provides database-backed analytics for high-cardinality
//! organization metrics that would be too expensive to track via Datadog.
//!
//! All costs use fixed scale 9 (nano-dollars) and USD currency.

use crate::common::RepositoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Summary metrics for an organization over a time period
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsSummary {
    pub total_requests: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    /// Total cost in USD (converted from nano-dollars)
    pub total_cost_usd: f64,
    /// Number of unique API keys used in the period
    pub unique_api_keys: i64,
    /// Requests with a recorded time to first token; the denominator for the
    /// `ttft_under_*` shares. See `ModelMetrics::ttft_measured_requests`.
    pub ttft_measured_requests: i64,
    /// Requests whose time to first token was strictly under 5000 ms
    pub ttft_under_5s_requests: i64,
    /// Requests whose time to first token was strictly under 10000 ms
    pub ttft_under_10s_requests: i64,
    /// Requests whose time to first token was strictly under 60000 ms
    pub ttft_under_60s_requests: i64,
}

/// Metrics breakdown by workspace
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WorkspaceMetrics {
    pub workspace_id: Uuid,
    pub workspace_name: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    /// Cost in USD
    pub cost_usd: f64,
}

/// Metrics breakdown by API key
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiKeyMetrics {
    pub api_key_id: Uuid,
    pub api_key_name: String,
    pub requests: i64,
    /// Cost in USD
    pub cost_usd: f64,
}

/// Metrics breakdown by model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelMetrics {
    pub model_name: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    /// Average time to first token in milliseconds
    pub avg_ttft_ms: Option<f64>,
    /// 95th percentile time to first token in milliseconds (approx. across hours: sample-weighted mean of hourly p95s; exact with credit_type)
    pub p95_ttft_ms: Option<f64>,
    /// Average inter-token latency in milliseconds
    pub avg_itl_ms: Option<f64>,
    /// 95th percentile inter-token latency in milliseconds (approx. across hours: sample-weighted mean of hourly p95s; exact with credit_type)
    pub p95_itl_ms: Option<f64>,
    /// Cost in USD
    pub cost_usd: f64,
    /// Requests with a recorded time to first token. Denominator for the `ttft_under_*`
    /// shares (share = ttft_under_Ns_requests / ttft_measured_requests); usually below
    /// `requests`: non-streamed requests and native passthrough streams (/v1/messages,
    /// native Responses) record no TTFT, and streams that end before usage arrives write no
    /// usage row at all. TTFT runs from request arrival to the first streamed chunk of any
    /// kind (possibly role-only or usage-only), not necessarily the first generated token.
    pub ttft_measured_requests: i64,
    /// Requests whose time to first token was strictly under 5000 ms
    pub ttft_under_5s_requests: i64,
    /// Requests whose time to first token was strictly under 10000 ms
    pub ttft_under_10s_requests: i64,
    /// Requests whose time to first token was strictly under 60000 ms
    pub ttft_under_60s_requests: i64,
}

/// Complete organization metrics response
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationMetrics {
    pub organization_id: Uuid,
    pub organization_name: String,
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub summary: MetricsSummary,
    pub by_workspace: Vec<WorkspaceMetrics>,
    pub by_api_key: Vec<ApiKeyMetrics>,
    pub by_model: Vec<ModelMetrics>,
}

/// Time series data point for analytics charts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeSeriesPoint {
    pub date: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
}

/// Time series response for organization metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeSeriesMetrics {
    pub organization_id: Uuid,
    pub organization_name: String,
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub granularity: String,
    pub data: Vec<TimeSeriesPoint>,
}

/// Repository trait for analytics queries
#[async_trait]
pub trait AnalyticsRepository: Send + Sync {
    /// Get organization metrics for a time range
    async fn get_organization_metrics(
        &self,
        org_id: Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        credit_type: Option<&str>,
    ) -> Result<OrganizationMetrics, RepositoryError>;

    /// Get time series metrics for an organization
    async fn get_organization_timeseries(
        &self,
        org_id: Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        granularity: &str,
        credit_type: Option<&str>,
    ) -> Result<TimeSeriesMetrics, RepositoryError>;
}

/// Analytics service implementation
pub struct AnalyticsService {
    repository: std::sync::Arc<dyn AnalyticsRepository>,
}

impl AnalyticsService {
    pub fn new(repository: std::sync::Arc<dyn AnalyticsRepository>) -> Self {
        Self { repository }
    }

    /// Get organization metrics for a time range
    ///
    /// Returns comprehensive metrics including:
    /// - Summary totals (requests, tokens, cost)
    /// - Breakdown by workspace
    /// - Breakdown by API key
    /// - Breakdown by model
    pub async fn get_organization_metrics(
        &self,
        org_id: Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        credit_type: Option<&str>,
    ) -> Result<OrganizationMetrics, super::AdminError> {
        self.repository
            .get_organization_metrics(org_id, start, end, credit_type)
            .await
            .map_err(analytics_error)
    }

    /// Get time series metrics for an organization
    ///
    /// Returns daily/weekly aggregations for charting
    pub async fn get_organization_timeseries(
        &self,
        org_id: Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        granularity: &str,
        credit_type: Option<&str>,
    ) -> Result<TimeSeriesMetrics, super::AdminError> {
        self.repository
            .get_organization_timeseries(org_id, start, end, granularity, credit_type)
            .await
            .map_err(analytics_error)
    }
}

/// Statement-budget cancellations become `Timeout` (HTTP 504), following
/// `ReportingUsageError::Timeout`; every other repository error stays internal.
fn analytics_error(error: RepositoryError) -> super::AdminError {
    match error {
        RepositoryError::QueryTimeout => super::AdminError::Timeout,
        other => super::AdminError::InternalError(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_timeout_becomes_admin_timeout() {
        assert!(matches!(
            analytics_error(RepositoryError::QueryTimeout),
            crate::admin::AdminError::Timeout
        ));
        assert!(matches!(
            analytics_error(RepositoryError::NotFound("x".to_string())),
            crate::admin::AdminError::InternalError(_)
        ));
    }
}
