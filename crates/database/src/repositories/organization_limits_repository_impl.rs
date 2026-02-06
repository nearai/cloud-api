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
        let limits = self.get_current_limits(organization_id).await?;

        if limits.is_empty() {
            return Ok(None);
        }

        let total_spend_limit: i64 = limits.iter().map(|l| l.spend_limit).sum();
        Ok(Some(OrganizationLimit {
            spend_limit: total_spend_limit,
        }))
    }
}
