use async_trait::async_trait;
use uuid::Uuid;

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
    async fn get_active_service_billing(
        &self,
        service_name: &str,
    ) -> anyhow::Result<Option<(Uuid, i64)>>;

    /// Insert usage row and update organization_balance. Idempotent when inference_id is set.
    async fn record_service_usage(&self, params: &RecordServiceUsageParams) -> anyhow::Result<()>;
}
