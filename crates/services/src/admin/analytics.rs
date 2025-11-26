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
    pub error_count: i64,
    pub error_rate_percent: f64,
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
    pub avg_latency_ms: Option<f64>,
    pub p95_latency_ms: Option<f64>,
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
    /// - Summary totals (requests, tokens, cost, errors)
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
}

