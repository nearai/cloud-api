pub mod ports;

use crate::service_usage::ports::{RecordServiceUsageParams, ServiceUsageRepositoryTrait};
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

    /// Check if a service is configured and active (for pricing). Returns Some((id, cost_per_unit)) or None.
    pub async fn get_active_service_pricing(
        &self,
        service_name: &str,
    ) -> anyhow::Result<Option<(Uuid, i64)>> {
        self.repo.get_active_service_pricing(service_name).await
    }

    /// Record one or more units of service usage. Looks up pricing by service_name.
    pub async fn record_service_usage(
        &self,
        organization_id: Uuid,
        workspace_id: Uuid,
        api_key_id: Uuid,
        service_name: &str,
        quantity: i32,
        inference_id: Option<Uuid>,
    ) -> Result<(), ServiceUsageError> {
        let Some((service_id, cost_per_unit)) = self
            .repo
            .get_active_service_pricing(service_name)
            .await
            .map_err(|e| ServiceUsageError::InternalError(e.to_string()))?
        else {
            return Err(ServiceUsageError::ServiceNotFound(service_name.to_string()));
        };

        self.record_service_usage_with_pricing(
            organization_id,
            workspace_id,
            api_key_id,
            service_id,
            cost_per_unit,
            quantity,
            inference_id,
        )
        .await
    }

    /// Record usage using pre-fetched (service_id, cost_per_unit). Use when caller already
    /// has pricing from get_active_service_pricing to avoid duplicate DB lookups and TOCTOU.
    pub async fn record_service_usage_with_pricing(
        &self,
        organization_id: Uuid,
        workspace_id: Uuid,
        api_key_id: Uuid,
        service_id: Uuid,
        cost_per_unit: i64,
        quantity: i32,
        inference_id: Option<Uuid>,
    ) -> Result<(), ServiceUsageError> {
        let total_cost = (quantity as i64)
            .checked_mul(cost_per_unit)
            .ok_or(ServiceUsageError::CostOverflow)?;

        self.repo
            .record_service_usage(&RecordServiceUsageParams {
                organization_id,
                workspace_id,
                api_key_id,
                service_id,
                quantity,
                total_cost,
                inference_id,
            })
            .await
            .map_err(|e| ServiceUsageError::InternalError(e.to_string()))?;

        Ok(())
    }
}
