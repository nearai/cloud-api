use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Known platform service names. Use these constants instead of string literals.
pub const SERVICE_NAME_WEB_SEARCH: &str = "web_search";

/// Billing unit for platform services.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceUnit {
    Request,
}

impl ServiceUnit {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceUnit::Request => "request",
        }
    }
}

impl TryFrom<&str> for ServiceUnit {
    type Error = String;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "request" => Ok(ServiceUnit::Request),
            _ => Err(format!("Unknown service unit: {}", s)),
        }
    }
}

/// Parameters for recording usage with pre-fetched pricing (avoids duplicate DB lookup).
#[derive(Debug, Clone)]
pub struct RecordServiceUsageWithPricingParams {
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub service_id: Uuid,
    pub cost_per_unit: i64,
    pub quantity: i32,
    pub inference_id: Option<Uuid>,
}

/// Parameters for recording one service usage row (and updating org balance).
#[derive(Debug, Clone)]
pub struct RecordServiceUsageParams {
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub service_id: Uuid,
    pub quantity: i32,
    pub total_cost: i64,
    pub inference_id: Option<Uuid>,
}

/// Port for recording platform service usage (e.g. web_search).
/// Implemented by database layer; used by ServiceUsageService.
#[async_trait]
pub trait ServiceUsageRepositoryTrait: Send + Sync {
    /// Returns (service_id, cost_per_unit) for active service or None if not found.
    async fn get_active_service_pricing(
        &self,
        service_name: &str,
    ) -> anyhow::Result<Option<(Uuid, i64)>>;

    /// Insert usage row and update organization_balance. Idempotent when inference_id is set.
    async fn record_service_usage(&self, params: &RecordServiceUsageParams) -> anyhow::Result<()>;
}
