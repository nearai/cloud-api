use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportingUsageSummaryFilters {
    pub organization_id: Uuid,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub workspace_id: Option<Uuid>,
    pub api_key_id: Option<Uuid>,
    pub model: Option<String>,
    pub inference_type: Option<String>,
    pub service_name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InferenceUsageSummary {
    pub totals: InferenceUsageTotals,
    pub by_workspace: Vec<InferenceWorkspaceSummary>,
    pub by_api_key: Vec<InferenceApiKeySummary>,
    pub by_model: Vec<InferenceModelSummary>,
    pub by_day: Vec<InferenceDaySummary>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InferenceUsageTotals {
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferenceWorkspaceSummary {
    pub workspace_id: Uuid,
    pub request_count: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferenceApiKeySummary {
    pub api_key_id: Uuid,
    pub request_count: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceModelSummary {
    pub model: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceDaySummary {
    pub day: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceUsageSummary {
    pub totals: ServiceUsageTotals,
    pub by_workspace: Vec<ServiceWorkspaceSummary>,
    pub by_api_key: Vec<ServiceApiKeySummary>,
    pub by_service: Vec<ServiceNameSummary>,
    pub by_day: Vec<ServiceDaySummary>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServiceUsageTotals {
    pub usage_count: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceWorkspaceSummary {
    pub workspace_id: Uuid,
    pub usage_count: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceApiKeySummary {
    pub api_key_id: Uuid,
    pub usage_count: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceNameSummary {
    pub service_name: String,
    pub usage_count: i64,
    pub quantity: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDaySummary {
    pub day: String,
    pub usage_count: i64,
    pub total_cost_nano_usd: i64,
}

#[async_trait::async_trait]
pub trait InferenceUsageSummaryRepository: Send + Sync {
    async fn summarize_inference_usage(
        &self,
        filters: &ReportingUsageSummaryFilters,
    ) -> anyhow::Result<InferenceUsageSummary>;
}

#[async_trait::async_trait]
pub trait ServiceUsageSummaryRepository: Send + Sync {
    async fn summarize_service_usage(
        &self,
        filters: &ReportingUsageSummaryFilters,
    ) -> anyhow::Result<ServiceUsageSummary>;
}
