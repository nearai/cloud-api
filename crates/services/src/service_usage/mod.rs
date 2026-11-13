pub mod ports;

pub use ports::ServiceUsageServiceTrait;
use ports::{
    RecordServiceUsageParams, RecordServiceUsageWithPricingParams, ServiceUsageRepositoryTrait,
};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ServiceUsageError {
    #[error("Service not found or inactive: {0}")]
    ServiceNotFound(String),
    #[error("Cost calculation overflow")]
    CostOverflow,
    #[error("Internal error: {0}")]
    InternalError(String),
}

/// Records platform-level service usage (e.g. web_search) and updates org balance.
#[derive(Clone)]
pub struct ServiceUsageService {
    repo: Arc<dyn ServiceUsageRepositoryTrait>,
}

impl ServiceUsageService {
    pub fn new(repo: Arc<dyn ServiceUsageRepositoryTrait>) -> Self {
        Self { repo }
    }
}

#[async_trait::async_trait]
impl ServiceUsageServiceTrait for ServiceUsageService {
    /// Check if a service is configured and active (for pricing). Returns Some((id, cost_per_unit)) or None.
    async fn get_active_service_pricing(
        &self,
        service_name: &str,
    ) -> anyhow::Result<Option<(Uuid, i64)>> {
        self.repo.get_active_service_pricing(service_name).await
    }

    /// Record usage using pre-fetched (service_id, cost_per_unit). Caller must obtain pricing
    /// from get_active_service_pricing to avoid duplicate DB lookups and TOCTOU.
    async fn record_service_usage_with_pricing(
        &self,
        params: &RecordServiceUsageWithPricingParams,
    ) -> Result<(), ServiceUsageError> {
        let total_cost = (params.quantity as i64)
            .checked_mul(params.cost_per_unit)
            .ok_or(ServiceUsageError::CostOverflow)?;

        self.repo
            .record_service_usage(&RecordServiceUsageParams {
                organization_id: params.organization_id,
                workspace_id: params.workspace_id,
                api_key_id: params.api_key_id,
                service_id: params.service_id,
                quantity: params.quantity,
                total_cost,
                inference_id: params.inference_id,
            })
            .await
            .map_err(|e| ServiceUsageError::InternalError(e.to_string()))?;

        Ok(())
    }
}
