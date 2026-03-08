use crate::repositories::{
    OrganizationServiceUsageRepository, RecordServiceUsageRequest, ServiceRepository,
};
use async_trait::async_trait;
use services::service_usage::ports::{RecordServiceUsageParams, ServiceUsageRepositoryTrait};
use std::sync::Arc;
use uuid::Uuid;

/// Implements ServiceUsageRepositoryTrait using ServiceRepository and OrganizationServiceUsageRepository.
pub struct ServiceUsageRepositoryImpl {
    service_repo: Arc<ServiceRepository>,
    usage_repo: Arc<OrganizationServiceUsageRepository>,
}

impl ServiceUsageRepositoryImpl {
    pub fn new(
        service_repo: Arc<ServiceRepository>,
        usage_repo: Arc<OrganizationServiceUsageRepository>,
    ) -> Self {
        Self {
            service_repo,
            usage_repo,
        }
    }
}

#[async_trait]
impl ServiceUsageRepositoryTrait for ServiceUsageRepositoryImpl {
    async fn get_active_service_billing(
        &self,
        service_name: &str,
    ) -> anyhow::Result<Option<(Uuid, i64)>> {
        let service = self.service_repo.get_active_by_name(service_name).await?;
        Ok(service.map(|s| (s.id, s.cost_per_unit)))
    }

    async fn record_service_usage(&self, params: &RecordServiceUsageParams) -> anyhow::Result<()> {
        let request = RecordServiceUsageRequest {
            organization_id: params.organization_id,
            workspace_id: params.workspace_id,
            api_key_id: params.api_key_id,
            service_id: params.service_id,
            quantity: params.quantity,
            total_cost: params.total_cost,
            inference_id: params.inference_id,
        };
        self.usage_repo.record_usage(&request).await?;
        Ok(())
    }
}
