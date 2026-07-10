use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceUsageReportCursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceUsageReportQuery {
    pub organization_id: Uuid,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub workspace_id: Option<Uuid>,
    pub api_key_id: Option<Uuid>,
    pub model: Option<String>,
    pub inference_type: Option<String>,
    pub limit: u16,
    pub cursor: Option<InferenceUsageReportCursor>,
    pub deadline: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceUsageHistoryQuery {
    pub organization_id: Uuid,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub workspace_id: Option<Uuid>,
    pub api_key_id: Option<Uuid>,
    pub limit: i64,
    pub offset: i64,
}

impl InferenceUsageReportQuery {
    pub const fn for_organization(organization_id: Uuid) -> Self {
        Self {
            organization_id,
            start_time: None,
            end_time: None,
            workspace_id: None,
            api_key_id: None,
            model: None,
            inference_type: None,
            limit: 100,
            cursor: None,
            deadline: None,
        }
    }
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
}
