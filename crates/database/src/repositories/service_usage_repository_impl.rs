use crate::models::OrganizationServiceUsageLog;
use crate::repositories::{
    OrganizationServiceUsageRepository, RecordServiceUsageRequest, ServiceRepository,
};
use async_trait::async_trait;
use services::service_usage::ports::{
    RecordServiceUsageParams, ServiceUsageLogEntry, ServiceUsageReportEntry,
    ServiceUsageReportFilters, ServiceUsageRepositoryTrait,
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
    async fn reserve_service(
        &self,
        params: &services::service_usage::ports::RecordServiceUsageWithPricingParams,
    ) -> anyhow::Result<Uuid> {
        anyhow::ensure!(
            params.quantity > 0 && params.cost_per_unit >= 0,
            "invalid service quote"
        );
        let amount = i64::from(params.quantity)
            .checked_mul(params.cost_per_unit)
            .ok_or_else(|| anyhow::anyhow!("service quote overflow"))?;
        let id = Uuid::new_v4();
        let mut client = self.usage_repo.pool.get().await?;
        let tx = client.transaction().await?;
        tx.batch_execute("SET LOCAL statement_timeout = '10s'; SET LOCAL lock_timeout = '5s'")
            .await?;
        tx.query_one(
            "SELECT id FROM reserve_usage_budget($1,$2,$3,$4,NULL,$5,$6,$7,$8,$9,$10)",
            &[
                &id,
                &params.organization_id,
                &params.workspace_id,
                &params.api_key_id,
                &params.service_id,
                &amount,
                &serde_json::json!({"cost_per_unit": params.cost_per_unit}),
                &serde_json::json!({"quantity": params.quantity}),
                &self.usage_repo.allocation_policy.priority,
                &self.usage_repo.allocation_policy.version,
            ],
        )
        .await
        .map_err(|error| {
            if matches!(error.code().map(|c| c.code()), Some("NBKEX" | "NBOEX")) {
                anyhow::Error::new(services::usage::UsageError::LimitExceeded(
                    "Insufficient available budget".into(),
                ))
            } else {
                error.into()
            }
        })?;
        let claimed: bool = tx
            .query_one(
                "SELECT dispatch_usage_budget($1,$2)",
                &[&params.organization_id, &id],
            )
            .await?
            .get(0);
        anyhow::ensure!(claimed, "service reservation is already dispatched");
        tx.commit().await?;
        Ok(id)
    }

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
        credit_type: Option<&str>,
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
            .list_for_org(organization_id, service_id, credit_type, limit, offset)
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
                credit_allocations: row.credit_allocations,
                funded_amount: row.funded_amount,
                unfunded_amount: row.unfunded_amount,
                allocation_policy_version: row.allocation_policy_version,
            })
            .collect();

        Ok((entries, total))
    }

    async fn list_usage_report(
        &self,
        filters: &ServiceUsageReportFilters,
    ) -> anyhow::Result<Vec<ServiceUsageReportEntry>> {
        self.usage_repo.list_reporting_usage(filters).await
    }
}
