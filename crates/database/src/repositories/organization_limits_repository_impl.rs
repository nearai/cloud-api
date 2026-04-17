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

        let now = chrono::Utc::now();

        // Sum non-expired limits; expired limits are excluded
        let total_spend_limit: i64 = limits
            .iter()
            .filter(|l| {
                l.credit_expires_at
                    .is_none_or(|expires_at| expires_at > now)
            })
            .map(|l| l.spend_limit)
            .sum();

        Ok(Some(OrganizationLimit {
            spend_limit: total_spend_limit,
            credit_expires_at: None, // Aggregated limits don't have a single expiry
        }))
    }
}
