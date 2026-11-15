use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceUsageHistoryQuery {
    pub organization_id: Uuid,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub workspace_id: Option<Uuid>,
    pub api_key_id: Option<Uuid>,
    pub credit_type: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceUsageReportRow {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub model: String,
    pub inference_type: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub service_tier: Option<String>,
    pub context_band: Option<String>,
    pub billing_details: Option<serde_json::Value>,
    pub total_tokens: i64,
    pub input_cost_nano_usd: i64,
    pub output_cost_nano_usd: i64,
    pub cache_read_cost_nano_usd: Option<i64>,
    pub total_cost_nano_usd: i64,
    pub response_id: Option<Uuid>,
    #[serde(skip)]
    pub provider_request_id: Option<String>,
    pub inference_id: Option<Uuid>,
    pub stop_reason: Option<String>,
    pub image_count: Option<i32>,
    pub credit_allocations: Option<Vec<super::CreditAllocation>>,
    pub funded_amount: Option<i64>,
    pub unfunded_amount: Option<i64>,
    pub allocation_policy_version: Option<String>,
}
