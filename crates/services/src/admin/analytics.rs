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
use uuid::Uuid;

/// Summary metrics for an organization over a time period
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSummary {
    pub total_requests: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    /// Total cost in USD (converted from nano-dollars)
    pub total_cost_usd: f64,
    /// Number of unique API keys used in the period
    pub unique_api_keys: i64,
}

/// Metrics breakdown by workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyMetrics {
    pub api_key_id: Uuid,
    pub api_key_name: String,
    pub requests: i64,
    /// Cost in USD
    pub cost_usd: f64,
}

/// Metrics breakdown by model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetrics {
    pub model_name: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    /// Average time to first token in milliseconds
    pub avg_ttft_ms: Option<f64>,
    /// 95th percentile time to first token in milliseconds
    pub p95_ttft_ms: Option<f64>,
    /// Average inter-token latency in milliseconds
    pub avg_itl_ms: Option<f64>,
    /// 95th percentile inter-token latency in milliseconds
    pub p95_itl_ms: Option<f64>,
    /// Cost in USD
    pub cost_usd: f64,
}

/// Complete organization metrics response
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Platform-wide metrics for admin dashboard
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformMetrics {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub total_users: i64,
    pub total_organizations: i64,
    pub total_requests: i64,
    /// Total consumed usage cost in USD (paid + granted)
    pub total_revenue_usd: f64,
    // --- Token volume (within the period) ---
    pub total_tokens: i64,
    pub total_cache_read_tokens: i64,
    // --- Acquisition (within the period) ---
    /// Users created in the period
    pub new_users: i64,
    /// Organizations created in the period
    pub new_organizations: i64,
    /// Organizations that issued ≥1 request in the period
    pub active_organizations: i64,
    /// Organizations with an active payment-type credit (paying customers)
    pub paying_organizations: i64,
    // --- Monetization split (consumption attributed by org class) ---
    /// Consumed cost from paying orgs (real revenue), USD
    pub paid_revenue_usd: f64,
    /// Consumed cost from grant-only orgs (free-credit burn), USD
    pub granted_revenue_usd: f64,
    // --- Verifiable / TEE differentiator split ---
    /// Consumed cost on verifiable (TEE-attested) models, USD
    pub verifiable_revenue_usd: f64,
    pub verifiable_requests: i64,
    /// Consumed cost on non-verifiable (external) models, USD
    pub external_revenue_usd: f64,
    pub external_requests: i64,
    // --- Reliability ---
    /// Share of requests whose stop_reason ∈ {provider_error, timeout}, 0.0–1.0
    pub error_rate: f64,
    /// 95th percentile time-to-first-token across the platform (ms)
    pub p95_ttft_ms: Option<f64>,
    pub top_models: Vec<TopModelMetrics>,
    pub top_organizations: Vec<TopOrganizationMetrics>,
}

/// One bucket of platform-wide time-series data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformTimeSeriesPoint {
    pub date: String,
    pub requests: i64,
    pub tokens: i64,
    pub cost_usd: f64,
    pub paid_cost_usd: f64,
    pub granted_cost_usd: f64,
    pub verifiable_cost_usd: f64,
    pub external_cost_usd: f64,
    pub active_organizations: i64,
    pub new_organizations: i64,
    pub new_users: i64,
}

/// Platform-wide time series response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformTimeSeriesMetrics {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub granularity: String,
    pub data: Vec<PlatformTimeSeriesPoint>,
}

/// Provisioned-credit / money-in breakdown by funding source
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingSourceBreakdown {
    pub source: String,
    pub paid_provisioned_usd: f64,
    pub org_count: i64,
}

/// Platform billing / credits summary (money-in lens, current snapshot)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingSummary {
    /// Sum of active payment-type spend limits (paid credits provisioned), USD
    pub paid_provisioned_usd: f64,
    /// Sum of active grant-type spend limits (free credits provisioned), USD
    pub granted_provisioned_usd: f64,
    /// All-time consumed cost across all orgs, USD
    pub total_consumed_usd: f64,
    /// All-time consumed cost from paying orgs, USD
    pub paid_consumed_usd: f64,
    /// Outstanding prepaid (deferred revenue): paid provisioned − paid consumed, clamped ≥0, USD
    pub unspent_paid_balance_usd: f64,
    pub paying_org_count: i64,
    pub granted_org_count: i64,
    /// Annualized run-rate: last-30d paid consumption × 12.17, USD
    pub run_rate_usd: f64,
    pub by_source: Vec<BillingSourceBreakdown>,
}

/// Per-model revenue breakdown entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRevenueEntry {
    pub model_name: String,
    pub revenue_usd: f64,
    pub paid_revenue_usd: f64,
    pub granted_revenue_usd: f64,
    pub requests: i64,
    pub tokens: i64,
    pub unique_orgs: i64,
    pub verifiable: bool,
    pub provider_type: Option<String>,
    pub avg_ttft_ms: Option<f64>,
    pub p95_ttft_ms: Option<f64>,
}

/// Full per-model revenue ranking for a period
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRevenueReport {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub models: Vec<ModelRevenueEntry>,
}

/// Per-organization spend/usage breakdown entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgRevenueEntry {
    pub organization_id: Uuid,
    pub organization_name: String,
    pub revenue_usd: f64,
    pub paid_revenue_usd: f64,
    pub granted_revenue_usd: f64,
    pub verifiable_revenue_usd: f64,
    pub external_revenue_usd: f64,
    pub requests: i64,
    pub tokens: i64,
    pub models_used: i64,
    /// Whether the org has an active payment-type credit (paying customer)
    pub is_paying: bool,
    pub last_usage_at: Option<DateTime<Utc>>,
}

/// Full per-organization spend/usage ranking for a period
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgRevenueReport {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub organizations: Vec<OrgRevenueEntry>,
}

/// Top model by request count
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopModelMetrics {
    pub model_name: String,
    pub requests: i64,
    pub revenue_usd: f64,
}

/// Top organization by spend
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopOrganizationMetrics {
    pub organization_id: Uuid,
    pub organization_name: String,
    pub requests: i64,
    pub spend_usd: f64,
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
    ) -> Result<OrganizationMetrics, RepositoryError>;

    /// Get platform-wide metrics for admin dashboard
    async fn get_platform_metrics(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<PlatformMetrics, RepositoryError>;

    /// Get time series metrics for an organization
    async fn get_organization_timeseries(
        &self,
        org_id: Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        granularity: &str,
    ) -> Result<TimeSeriesMetrics, RepositoryError>;

    /// Get platform-wide time series for admin dashboards
    async fn get_platform_timeseries(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        granularity: &str,
    ) -> Result<PlatformTimeSeriesMetrics, RepositoryError>;

    /// Get the platform billing / credits summary (money-in lens)
    async fn get_billing_summary(
        &self,
        as_of: DateTime<Utc>,
    ) -> Result<BillingSummary, RepositoryError>;

    /// Get the full per-model revenue ranking for a period
    async fn get_model_revenue(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<ModelRevenueReport, RepositoryError>;

    /// Get the full per-organization spend/usage ranking for a period
    async fn get_org_revenue(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<OrgRevenueReport, RepositoryError>;
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
    ) -> Result<OrganizationMetrics, super::AdminError> {
        self.repository
            .get_organization_metrics(org_id, start, end)
            .await
            .map_err(|e| super::AdminError::InternalError(e.to_string()))
    }

    /// Get platform-wide metrics for admin dashboard
    ///
    /// Returns aggregated metrics across all organizations:
    /// - Total users and organizations
    /// - Total requests and revenue
    /// - Top models by usage
    /// - Top organizations by spend
    pub async fn get_platform_metrics(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<PlatformMetrics, super::AdminError> {
        self.repository
            .get_platform_metrics(start, end)
            .await
            .map_err(|e| super::AdminError::InternalError(e.to_string()))
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
    ) -> Result<TimeSeriesMetrics, super::AdminError> {
        self.repository
            .get_organization_timeseries(org_id, start, end, granularity)
            .await
            .map_err(|e| super::AdminError::InternalError(e.to_string()))
    }

    /// Get platform-wide time series for growth/mix trend charts
    pub async fn get_platform_timeseries(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        granularity: &str,
    ) -> Result<PlatformTimeSeriesMetrics, super::AdminError> {
        self.repository
            .get_platform_timeseries(start, end, granularity)
            .await
            .map_err(|e| super::AdminError::InternalError(e.to_string()))
    }

    /// Get the platform billing / credits summary (money-in lens)
    pub async fn get_billing_summary(
        &self,
        as_of: DateTime<Utc>,
    ) -> Result<BillingSummary, super::AdminError> {
        self.repository
            .get_billing_summary(as_of)
            .await
            .map_err(|e| super::AdminError::InternalError(e.to_string()))
    }

    /// Get the full per-model revenue ranking for a period
    pub async fn get_model_revenue(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<ModelRevenueReport, super::AdminError> {
        self.repository
            .get_model_revenue(start, end)
            .await
            .map_err(|e| super::AdminError::InternalError(e.to_string()))
    }

    /// Get the full per-organization spend/usage ranking for a period
    pub async fn get_org_revenue(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<OrgRevenueReport, super::AdminError> {
        self.repository
            .get_org_revenue(start, end)
            .await
            .map_err(|e| super::AdminError::InternalError(e.to_string()))
    }
}
