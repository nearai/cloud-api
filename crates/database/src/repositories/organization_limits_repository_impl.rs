use crate::repositories::OrganizationLimitsRepository;
use services::usage::ports::{OrganizationCreditLimit, OrganizationLimit};
use uuid::Uuid;

/// Trait implementation adapter for OrganizationLimitsRepository
#[async_trait::async_trait]
impl services::usage::ports::OrganizationLimitsRepository for OrganizationLimitsRepository {
    async fn get_current_limits(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationLimit>> {
        let limits = self.get_current_limits(organization_id).await?;

        if limits.is_empty() {
            return Ok(None);
        }

        let total_spend_limit: i64 = limits.iter().map(|l| l.spend_limit).sum();
        Ok(Some(OrganizationLimit {
            spend_limit: total_spend_limit,
        }))
    }

    async fn get_current_limit_breakdown(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Vec<OrganizationCreditLimit>> {
        let limits = self.get_current_limits(organization_id).await?;
        Ok(limits
            .into_iter()
            .map(|limit| OrganizationCreditLimit {
                credit_type: limit.credit_type,
                source: limit.source,
                amount: limit.spend_limit,
                currency: limit.currency,
            })
            .collect())
    }
}
