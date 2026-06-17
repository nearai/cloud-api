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
    /// When this response was computed. The `total_users`/`total_organizations`/
    /// `paying_organizations` fields are **current snapshots as of this time**, not
    /// period-bound like the rest.
    pub generated_at: DateTime<Utc>,
    /// Snapshot (as of `generated_at`).
    pub total_users: i64,
    /// Snapshot (as of `generated_at`).
    pub total_organizations: i64,
    pub total_requests: i64,
    /// Total **consumed usage cost** in USD over the period (inference only).
    /// NOT recognized revenue: grant/payment funding is not separable without
    /// per-request attribution (tracked in #704).
    pub total_consumed_usd: f64,
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
    /// Snapshot (as of `generated_at`): orgs with an active payment-type credit.
    /// NOTE: does not yet count postpaid/invoiced customers (see #704).
    pub paying_organizations: i64,
    // --- Verifiable / TEE split. NOTE: computed by joining usage to *current*
    //     model metadata, so historical periods can shift if a model's verifiable
    //     flag changes. A usage-time snapshot is tracked in #704. ---
    /// Consumed cost on verifiable (TEE-attested) models, USD
    pub verifiable_consumed_usd: f64,
    pub verifiable_requests: i64,
    /// Consumed cost on non-verifiable models, USD (NOT necessarily third-party)
    pub non_verifiable_consumed_usd: f64,
    pub non_verifiable_requests: i64,
    // --- Reliability ---
    /// Share of requests whose stop_reason ∈ {provider_error, timeout}, 0.0–1.0
    pub provider_error_or_timeout_rate: f64,
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
    /// Non-verifiable consumed cost (NOT necessarily third-party), USD
    pub non_verifiable_cost_usd: f64,
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
    pub generated_at: DateTime<Utc>,
    /// Sum of active payment-type spend limits (caps), USD
    pub active_paid_credit_limit_usd: f64,
    /// Sum of active grant-type spend limits (caps), USD
    pub active_grant_credit_limit_usd: f64,
    /// All-time consumed cost across all orgs, USD — **all usage** (from
    /// organization_balance: inference + services). `inference_consumed_usd +
    /// service_consumed_usd` reconcile to this.
    pub total_consumed_usd: f64,
    /// All-time inference consumed cost, USD (organization_usage_log)
    pub inference_consumed_usd: f64,
    /// All-time service consumed cost, USD (organization_service_usage_log, e.g. web_search)
    pub service_consumed_usd: f64,
    pub paying_org_count: i64,
    pub granted_org_count: i64,
    pub by_source: Vec<BillingSourceBreakdown>,
}

/// Per-model consumption breakdown entry
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelRevenueEntry {
    pub model_name: String,
    /// Consumed usage cost, USD
    pub consumed_cost_usd: f64,
    pub requests: i64,
    pub tokens: i64,
    pub unique_orgs: i64,
    /// Current model metadata (may differ from when the usage occurred).
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
    pub consumed_cost_usd: f64,
    pub verifiable_consumed_usd: f64,
    pub non_verifiable_consumed_usd: f64,
    pub requests: i64,
    pub tokens: i64,
    pub models_used: i64,
    /// Current snapshot: org has an active payment-type credit (not historical;
    /// does not yet count postpaid — see #704).
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
    /// Parse from the API `sort` query param. `None` → `Revenue` (default);
    /// an unknown value is an error (handlers return 400).
    pub fn from_query(s: Option<&str>) -> Result<Self, String> {
        match s {
            None | Some("revenue") => Ok(RevenueSort::Revenue),
            Some("requests") => Ok(RevenueSort::Requests),
            Some("tokens") => Ok(RevenueSort::Tokens),
            Some(other) => Err(format!(
                "invalid sort '{other}'; expected one of: revenue, requests, tokens"
            )),
        }
    }
}

/// Filters + pagination for the per-model revenue ranking.
#[derive(Debug, Clone)]
pub struct ModelRevenueQuery {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub verifiable: Option<bool>,
    /// Allowlisted provider type ("vllm" | "external"); validated in the handler.
    pub provider_type: Option<String>,
    /// Case-insensitive substring match on model name.
    pub model_search: Option<String>,
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
    /// Case-insensitive substring match on organization name.
    pub search: Option<String>,
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

/// One time-bucket × model row for the consumption timeseries.
///
/// Models outside the top-N are collapsed to `model_label = "Other"` server-side.
/// `model_id` is `None` for the "Other" bucket.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelConsumptionPoint {
    /// ISO 8601 bucket start (truncated to granularity)
    pub bucket: String,
    /// Current canonical model name (from `models.model_name`), or "Other"
    pub model_label: String,
    /// Consumed usage cost for this (bucket, model) pair, USD
    pub consumed_cost_usd: f64,
    pub requests: i64,
    /// input_tokens + output_tokens
    pub tokens: i64,
}

/// Per-model consumption timeseries response.
///
/// Top N models by total period cost are returned as separate series; the rest
/// are collapsed into a single "Other" series. The frontend can zero-fill missing
/// (bucket, model) pairs — the query only emits rows where usage > 0.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelConsumptionTimeseries {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub granularity: String,
    /// Ordered list of model labels in the response (top-N + "Other" if present)
    pub model_labels: Vec<String>,
    pub data: Vec<ModelConsumptionPoint>,
}

/// Query params for the model consumption timeseries endpoint.
#[derive(Debug, Clone)]
pub struct ModelConsumptionTimeseriesQuery {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Allowlisted granularity — must come from `allowlisted_date_trunc`.
    pub granularity: String,
    /// Number of top models to return as separate series (rest → "Other"). Default 15.
    pub top_n: i64,
}

/// One time-bucket of platform-wide performance metrics.
///
/// TTFT percentiles cover **streaming requests only** (ttft_ms IS NOT NULL).
/// `ttft_sample_count` exposes the denominator so callers know the coverage fraction.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PerformancePoint {
    /// ISO 8601 bucket start (truncated to granularity)
    pub bucket: String,
    pub requests: i64,
    /// input_tokens + output_tokens
    pub total_tokens: i64,
    /// output_tokens only (generation throughput, prompt-inflation-free)
    pub output_tokens: i64,
    /// Number of requests with ttft_ms recorded (streaming only). Use this as the
    /// denominator when interpreting TTFT percentiles.
    pub ttft_sample_count: i64,
    /// 50th-percentile TTFT, ms (streaming requests only; None if no samples)
    pub p50_ttft_ms: Option<f64>,
    /// 95th-percentile TTFT, ms (streaming requests only; None if no samples)
    pub p95_ttft_ms: Option<f64>,
    /// 99th-percentile TTFT, ms (streaming requests only; None if no samples)
    pub p99_ttft_ms: Option<f64>,
    /// stop_reason IN ('provider_error','timeout') / requests WHERE stop_reason IS NOT NULL.
    /// Excludes pre-V0037 rows (stop_reason IS NULL) from both numerator and denominator.
    pub error_rate: Option<f64>,
}

/// Platform-wide performance timeseries response.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PerformanceTimeseries {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub granularity: String,
    /// Optional model filter applied (None = platform-wide)
    pub model_filter: Option<String>,
    pub data: Vec<PerformancePoint>,
}

/// Per-model revenue density row.
///
/// All rate fields are in USD/second, derived from 1-minute buckets.
/// Percentiles are computed over **active minutes only** (buckets with >0 revenue)
/// to reflect the rate during actual serving windows, not idle time.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RevenueDensityModelRow {
    pub model_name: String,
    /// P50 revenue rate during active minutes, USD/s
    pub p50_usd_per_sec: f64,
    /// P95 revenue rate during active minutes, USD/s
    pub p95_usd_per_sec: f64,
    /// P99 revenue rate during active minutes, USD/s
    pub p99_usd_per_sec: f64,
    /// Maximum observed revenue rate, USD/s
    pub peak_usd_per_sec: f64,
    /// Annualized revenue if p99 rate were constant: p99 × 86400 × 365, USD
    pub p99_annualized_usd: f64,
    /// Annualized revenue if peak rate were constant: peak × 86400 × 365, USD
    pub peak_annualized_usd: f64,
    /// Number of 1-minute buckets with revenue > 0 in the period
    pub active_minutes: i64,
}

/// Platform-wide revenue density report with per-model breakdown.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RevenueDensityReport {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    /// Total 1-minute buckets sampled in the period
    pub sampled_minutes: i64,
    /// Buckets with any revenue > 0
    pub active_minutes: i64,
    // Platform-wide percentiles (over active minutes, all models combined)
    pub p50_usd_per_sec: f64,
    pub p95_usd_per_sec: f64,
    pub p99_usd_per_sec: f64,
    pub peak_usd_per_sec: f64,
    pub p99_annualized_usd: f64,
    pub peak_annualized_usd: f64,
    /// Per-model breakdown, sorted by total consumed cost DESC
    pub by_model: Vec<RevenueDensityModelRow>,
}

/// Query params for the revenue density endpoint.
#[derive(Debug, Clone)]
pub struct RevenueDensityQuery {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// Query params for the performance timeseries endpoint.
#[derive(Debug, Clone)]
pub struct PerformanceTimeseriesQuery {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Allowlisted granularity — must come from `allowlisted_date_trunc`.
    pub granularity: String,
    /// Optional exact model name filter.
    pub model_name: Option<String>,
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

    /// Per-model consumption timeseries (top-N models + "Other" collapsed bucket)
    async fn get_model_consumption_timeseries(
        &self,
        query: ModelConsumptionTimeseriesQuery,
    ) -> Result<ModelConsumptionTimeseries, RepositoryError>;

    /// Platform-wide (or per-model) performance timeseries: TTFT percentiles, throughput, error rate
    async fn get_performance_timeseries(
        &self,
        query: PerformanceTimeseriesQuery,
    ) -> Result<PerformanceTimeseries, RepositoryError>;

    /// Revenue density percentiles (p50/p95/p99/peak USD/s) over 1-minute buckets,
    /// platform-wide and per-model.
    async fn get_revenue_density(
        &self,
        query: RevenueDensityQuery,
    ) -> Result<RevenueDensityReport, RepositoryError>;
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

    /// Per-model consumption timeseries (top-N + "Other")
    pub async fn get_model_consumption_timeseries(
        &self,
        query: ModelConsumptionTimeseriesQuery,
    ) -> Result<ModelConsumptionTimeseries, super::AdminError> {
        self.repository
            .get_model_consumption_timeseries(query)
            .await
            .map_err(|e| super::AdminError::InternalError(e.to_string()))
    }

    /// Platform-wide (or per-model) performance timeseries
    pub async fn get_performance_timeseries(
        &self,
        query: PerformanceTimeseriesQuery,
    ) -> Result<PerformanceTimeseries, super::AdminError> {
        self.repository
            .get_performance_timeseries(query)
            .await
            .map_err(|e| super::AdminError::InternalError(e.to_string()))
    }

    /// Revenue density percentiles (p50/p95/p99/peak USD/s)
    pub async fn get_revenue_density(
        &self,
        query: RevenueDensityQuery,
    ) -> Result<RevenueDensityReport, super::AdminError> {
        self.repository
            .get_revenue_density(query)
            .await
            .map_err(|e| super::AdminError::InternalError(e.to_string()))
    }
}
