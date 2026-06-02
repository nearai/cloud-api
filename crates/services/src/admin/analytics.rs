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
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PlatformMetrics {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub total_users: i64,
    pub total_organizations: i64,
    pub total_requests: i64,
    /// Total **consumed usage cost** in USD. NOT recognized revenue: grant- and
    /// payment-funded usage are not separable without per-request attribution.
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
    /// Current snapshot: organizations that have an active payment-type credit
    /// (a current count, not a historical attribution).
    pub paying_organizations: i64,
    // --- Verifiable / TEE differentiator split ---
    /// Consumed cost on verifiable (TEE-attested) models, USD
    pub verifiable_revenue_usd: f64,
    pub verifiable_requests: i64,
    /// Consumed cost on non-verifiable models, USD
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
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PlatformTimeSeriesPoint {
    pub date: String,
    pub requests: i64,
    pub tokens: i64,
    /// Consumed usage cost for the bucket, USD
    pub cost_usd: f64,
    pub verifiable_cost_usd: f64,
    pub external_cost_usd: f64,
    pub active_organizations: i64,
    pub new_organizations: i64,
    pub new_users: i64,
}

/// Platform-wide time series response
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PlatformTimeSeriesMetrics {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub granularity: String,
    pub data: Vec<PlatformTimeSeriesPoint>,
}

/// Active credit limit by funding source (caps, not payments).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BillingSourceBreakdown {
    pub source: String,
    /// Sum of active payment-type spend limits (caps) for this source, USD
    pub paid_credit_limit_usd: f64,
    pub org_count: i64,
}

/// Platform billing summary — credit LIMITS (caps) and consumption.
///
/// These are spend-limit ceilings from `organization_limits_history`, NOT payments
/// or cash received. Real money-in (Stripe top-ups) lives in the billing service
/// (nearai-cloud-ui), not in cloud-api.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BillingSummary {
    /// Sum of active payment-type spend limits (caps), USD
    pub active_paid_credit_limit_usd: f64,
    /// Sum of active grant-type spend limits (caps), USD
    pub active_grant_credit_limit_usd: f64,
    /// All-time consumed cost across all orgs, USD (from organization_balance)
    pub total_consumed_usd: f64,
    pub paying_org_count: i64,
    pub granted_org_count: i64,
    pub by_source: Vec<BillingSourceBreakdown>,
}

/// Per-model consumption breakdown entry
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelRevenueEntry {
    pub model_name: String,
    /// Consumed usage cost, USD
    pub revenue_usd: f64,
    pub requests: i64,
    pub tokens: i64,
    pub unique_orgs: i64,
    pub verifiable: bool,
    pub provider_type: Option<String>,
    pub avg_ttft_ms: Option<f64>,
    pub p95_ttft_ms: Option<f64>,
}

/// Paginated per-model consumption ranking for a period
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelRevenueReport {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub data: Vec<ModelRevenueEntry>,
    /// Total matching models (before limit/offset)
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Per-organization consumption breakdown entry
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrgRevenueEntry {
    pub organization_id: Uuid,
    pub organization_name: String,
    /// Consumed usage cost, USD
    pub revenue_usd: f64,
    pub verifiable_revenue_usd: f64,
    pub external_revenue_usd: f64,
    pub requests: i64,
    pub tokens: i64,
    pub models_used: i64,
    /// Current snapshot: org has an active payment-type credit (not historical).
    pub is_paying: bool,
    pub last_usage_at: Option<DateTime<Utc>>,
}

/// Paginated per-organization consumption ranking for a period
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrgRevenueReport {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub data: Vec<OrgRevenueEntry>,
    /// Total matching organizations (before limit/offset)
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Sort key for paginated revenue rankings. Maps to a column via an allowlist
/// in the repository (never interpolate user input into SQL).
#[derive(Debug, Clone, Copy)]
pub enum RevenueSort {
    Revenue,
    Requests,
    Tokens,
}

impl RevenueSort {
    /// Parse from the API `sort` query param; unknown/None → `Revenue`.
    pub fn from_query(s: Option<&str>) -> Self {
        match s {
            Some("requests") => RevenueSort::Requests,
            Some("tokens") => RevenueSort::Tokens,
            _ => RevenueSort::Revenue,
        }
    }
}

/// Filters + pagination for the per-model revenue ranking.
#[derive(Debug, Clone)]
pub struct ModelRevenueQuery {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub verifiable: Option<bool>,
    pub provider_type: Option<String>,
    pub sort: RevenueSort,
    pub limit: i64,
    pub offset: i64,
}

/// Filters + pagination for the per-organization revenue ranking.
#[derive(Debug, Clone)]
pub struct OrgRevenueQuery {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Filter to current paying / non-paying orgs (by active payment credit).
    pub paying: Option<bool>,
    pub sort: RevenueSort,
    pub limit: i64,
    pub offset: i64,
}

/// Top model by request count
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TopModelMetrics {
    pub model_name: String,
    pub requests: i64,
    pub revenue_usd: f64,
}

/// Top organization by spend
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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

    /// Get the platform billing summary (credit limits + consumption)
    async fn get_billing_summary(&self) -> Result<BillingSummary, RepositoryError>;

    /// Get a paginated/filtered per-model consumption ranking
    async fn get_model_revenue(
        &self,
        query: ModelRevenueQuery,
    ) -> Result<ModelRevenueReport, RepositoryError>;

    /// Get a paginated/filtered per-organization consumption ranking
    async fn get_org_revenue(
        &self,
        query: OrgRevenueQuery,
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

    /// Get the platform billing summary (credit limits + consumption)
    pub async fn get_billing_summary(&self) -> Result<BillingSummary, super::AdminError> {
        self.repository
            .get_billing_summary()
            .await
            .map_err(|e| super::AdminError::InternalError(e.to_string()))
    }

    /// Get a paginated/filtered per-model consumption ranking
    pub async fn get_model_revenue(
        &self,
        query: ModelRevenueQuery,
    ) -> Result<ModelRevenueReport, super::AdminError> {
        self.repository
            .get_model_revenue(query)
            .await
            .map_err(|e| super::AdminError::InternalError(e.to_string()))
    }

    /// Get a paginated/filtered per-organization consumption ranking
    pub async fn get_org_revenue(
        &self,
        query: OrgRevenueQuery,
    ) -> Result<OrgRevenueReport, super::AdminError> {
        self.repository
            .get_org_revenue(query)
            .await
            .map_err(|e| super::AdminError::InternalError(e.to_string()))
    }
}
