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
    pub total_revenue_usd: f64,
    pub top_models: Vec<TopModelMetrics>,
    pub top_organizations: Vec<TopOrganizationMetrics>,
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
}
