use crate::repositories::OrganizationLimitsRepository;
use services::usage::ports::OrganizationLimit;
use uuid::Uuid;

/// Trait implementation adapter for OrganizationLimitsRepository
#[async_trait::async_trait]
impl services::usage::ports::OrganizationLimitsRepository for OrganizationLimitsRepository {
    async fn get_current_limits(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationLimit>> {
        let limit = self.get_current_limits(organization_id).await?;

        Ok(limit.map(|l| OrganizationLimit {
            spend_limit_amount: l.spend_limit_amount,
            spend_limit_scale: l.spend_limit_scale,
            spend_limit_currency: l.spend_limit_currency,
        }))
    }
}

