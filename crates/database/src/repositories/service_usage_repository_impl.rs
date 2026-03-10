use crate::models::OrganizationServiceUsageLog;
use crate::repositories::{
    OrganizationServiceUsageRepository, RecordServiceUsageRequest, ServiceRepository,
};
use async_trait::async_trait;
use services::service_usage::ports::{
    RecordServiceUsageParams, ServiceUsageLogEntry, ServiceUsageRepositoryTrait,
};
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
    async fn get_active_service_pricing(
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

    async fn list_usage_logs(
        &self,
        organization_id: Uuid,
        service_name: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<(Vec<ServiceUsageLogEntry>, i64)> {
        // Resolve service_name to service_id if provided; include inactive services so that
        // history and billing remain accurate even after a service is deactivated.
        let service_id = if let Some(name) = service_name {
            match self.service_repo.get_by_name_any_status(name).await? {
                Some(service) => Some(service.id),
                None => {
                    // No service with this name; return empty result.
                    return Ok((Vec::new(), 0));
                }
            }
        } else {
            None
        };

        let (rows, total) = self
            .usage_repo
            .list_for_org(organization_id, service_id, limit, offset)
            .await?;

        let entries = rows
            .into_iter()
            .map(|row: OrganizationServiceUsageLog| ServiceUsageLogEntry {
                id: row.id,
                organization_id: row.organization_id,
                workspace_id: row.workspace_id,
                api_key_id: row.api_key_id,
                service_id: row.service_id,
                quantity: row.quantity,
                total_cost: row.total_cost,
                inference_id: row.inference_id,
                created_at: row.created_at,
            })
            .collect();

        Ok((entries, total))
    }
}
