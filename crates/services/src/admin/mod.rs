pub mod analytics;
pub mod ports;

pub use analytics::{
    AnalyticsRepository, AnalyticsService, ApiKeyMetrics, MetricsSummary, ModelMetrics,
    OrganizationMetrics, TimeSeriesMetrics, TimeSeriesPoint, WorkspaceMetrics,
};
pub mod infra;
pub mod pricing_scheduler;
pub use infra::{InfraService, InfraSummary, ModelGpuAllocation};
pub use ports::{PlatformServiceInfo, *};
pub use pricing_scheduler::ModelPricingScheduler;
use std::sync::Arc;

/// Value admin reads return in place of a stored `providerConfig.api_key`.
pub const REDACTED_API_KEY: &str = "***";

pub struct AdminServiceImpl {
    repository: Arc<dyn AdminRepository>,
}

impl AdminServiceImpl {
    pub fn new(repository: Arc<dyn AdminRepository>) -> Self {
        Self { repository }
    }
}

#[async_trait::async_trait]
impl AdminService for AdminServiceImpl {
    async fn list_api_keys(
        &self,
        filters: AdminApiKeyFilters,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminApiKeyInfo>, i64), AdminError> {
        if let (Some(created_after), Some(created_before)) =
            (filters.created_after, filters.created_before)
        {
            if created_after > created_before {
                return Err(AdminError::InvalidParams(
                    "created_after must be before created_before".to_string(),
                ));
            }
        }

        let (api_keys_result, total_result) = tokio::join!(
            self.repository.list_all_api_keys(&filters, limit, offset),
            self.repository.count_all_api_keys(&filters)
        );

        let api_keys = api_keys_result.map_err(|e| AdminError::InternalError(e.to_string()))?;
        let total = total_result.map_err(|e| AdminError::InternalError(e.to_string()))?;

        Ok((api_keys, total))
    }

    async fn list_organization_members(
        &self,
        organization_id: uuid::Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminOrganizationMemberInfo>, i64), AdminError> {
        let (members_result, total_result) = tokio::join!(
            self.repository
                .list_organization_members(organization_id, limit, offset),
            self.repository.count_organization_members(organization_id)
        );

        let members = members_result.map_err(|e| AdminError::InternalError(e.to_string()))?;
        let total = total_result.map_err(|e| AdminError::InternalError(e.to_string()))?;

        // Distinguish "organization does not exist" (404) from "exists but has
        // no members" (200 with an empty list). Only pay for the existence
        // round-trip when the count is zero — in the common case (org exists,
        // has members) `total > 0` proves existence and we skip it.
        if total == 0 {
            let exists = self
                .repository
                .organization_exists(organization_id)
                .await
                .map_err(|e| AdminError::InternalError(e.to_string()))?;
            if !exists {
                return Err(AdminError::OrganizationNotFound(
                    organization_id.to_string(),
                ));
            }
        }

        Ok((members, total))
    }
}
