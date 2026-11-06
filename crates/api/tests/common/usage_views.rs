//! Local copies of the usage response shapes the e2e tests assert on.
//!
//! The usage console routes that returned these are gone; `seeding` reads the same
//! values from the database, and these structs carry them to the tests.

use services::usage::CreditAllocation;

#[derive(Debug)]
pub struct OrganizationBalanceResponse {
    pub organization_id: String,
    pub legacy_unattributed_amount: i64,
    pub total_spent: i64,
    pub total_spent_display: String,
    pub spend_limit: Option<i64>,
    pub spend_limit_display: Option<String>,
    pub remaining: Option<i64>,
    pub remaining_display: Option<String>,
    pub last_usage_at: Option<String>,
    pub total_requests: i64,
    pub total_tokens: i64,
    pub updated_at: String,
    pub credit_limits: Vec<CreditLimitBreakdownResponse>,
    pub unfunded_amount: i64,
}

#[derive(Debug)]
pub struct CreditLimitBreakdownResponse {
    pub credit_type: String,
    pub source: Option<String>,
    pub amount: i64,
    pub transferred_out: i64,
    pub effective_capacity: i64,
    pub consumed: i64,
    pub available: i64,
    pub currency: String,
}

#[derive(Debug)]
pub struct UsageHistoryEntryResponse {
    pub id: String,
    pub workspace_id: String,
    pub api_key_id: String,
    pub model: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cache_read_tokens: i32,
    pub cache_write_tokens: i32,
    pub service_tier: Option<String>,
    pub context_band: Option<String>,
    pub billing_details: Option<serde_json::Value>,
    pub total_tokens: i32,
    pub total_cost: i64,
    pub total_cost_display: String,
    pub inference_type: String,
    pub created_at: String,
    pub stop_reason: Option<String>,
    pub response_id: Option<String>,
    pub provider_request_id: Option<String>,
    pub inference_id: Option<String>,
    pub image_count: Option<i32>,
    pub credit_allocations: Option<Vec<CreditAllocation>>,
    pub funded_amount: Option<i64>,
    pub unfunded_amount: Option<i64>,
    pub allocation_policy_version: Option<String>,
}

#[derive(Debug)]
pub struct UsageHistoryResponse {
    pub data: Vec<UsageHistoryEntryResponse>,
    pub total: usize,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug)]
pub struct ServiceUsageEntryResponse {
    pub id: String,
    pub organization_id: String,
    pub workspace_id: String,
    pub api_key_id: String,
    pub service_id: String,
    pub quantity: i32,
    pub total_cost: i64,
    pub total_cost_display: String,
    pub created_at: String,
    pub credit_allocations: Option<Vec<CreditAllocation>>,
    pub funded_amount: Option<i64>,
    pub unfunded_amount: Option<i64>,
}

#[derive(Debug)]
pub struct ServiceUsageHistoryResponse {
    pub data: Vec<ServiceUsageEntryResponse>,
    pub total: usize,
    pub limit: i64,
    pub offset: i64,
}
