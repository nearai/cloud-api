use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{ReportingUsageRowSource, ReportingUsageSource};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportingUsageExportResponse {
    pub data: Vec<ReportingUsageExportRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportingUsageExportRow {
    pub source: ReportingUsageRowSource,
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub total_cost_nano_usd: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference: Option<ReportingInferenceUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<ReportingServiceUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportingInferenceUsage {
    pub model: String,
    pub inference_type: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub input_cost_nano_usd: i64,
    pub output_cost_nano_usd: i64,
    /// Omitted when Cloud API has not persisted a separate cache-read cost split.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_cost_nano_usd: Option<i64>,
    pub total_cost_nano_usd: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<Uuid>,
    /// Customer-facing request correlation id for v1 reporting.
    ///
    /// Reporting intentionally excludes upstream provider request ids and provider-attribution fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_count: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportingServiceUsage {
    pub service_name: String,
    pub quantity: i64,
    pub total_cost_nano_usd: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportingUsageSummaryResponse {
    pub source: ReportingUsageSource,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub totals: ReportingUsageTotals,
    pub by_workspace: Vec<ReportingWorkspaceSummary>,
    pub by_api_key: Vec<ReportingApiKeySummary>,
    pub by_model: Vec<ReportingModelSummary>,
    pub by_service: Vec<ReportingServiceSummary>,
    pub by_day: Vec<ReportingDaySummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportingUsageTotals {
    pub request_count: i64,
    pub service_usage_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub inference_cost_nano_usd: i64,
    pub service_cost_nano_usd: i64,
    pub total_cost_nano_usd: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportingWorkspaceSummary {
    pub workspace_id: Uuid,
    pub request_count: i64,
    pub service_usage_count: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportingApiKeySummary {
    pub api_key_id: Uuid,
    pub request_count: i64,
    pub service_usage_count: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportingModelSummary {
    pub model: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportingServiceSummary {
    pub service_name: String,
    pub usage_count: i64,
    pub quantity: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportingDaySummary {
    pub day: String,
    pub request_count: i64,
    pub service_usage_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub inference_cost_nano_usd: i64,
    pub service_cost_nano_usd: i64,
    pub total_cost_nano_usd: i64,
}
