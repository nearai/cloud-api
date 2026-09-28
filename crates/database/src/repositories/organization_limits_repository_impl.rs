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
        let (limits, unfunded, unattributed) =
            self.get_current_credit_status(organization_id).await?;

        if limits.is_empty() {
            return Ok(None);
        }

        // All active types, including postpay, authorize usage. Postpay is a
        // contract safety ceiling rather than prepaid cash, but excluding it
        // here would leave a postpay-only organization unable to run requests.
        let total_spend_limit = limits
            .iter()
            .map(|status| status.limit.spend_limit)
            .fold(0_i64, i64::saturating_add);
        let available = limits
            .iter()
            .map(|status| status.available)
            .fold(0_i64, i64::saturating_add)
            .saturating_sub(unattributed)
            .max(0);
        Ok(Some(OrganizationLimit {
            spend_limit: total_spend_limit,
            available,
            unfunded,
        }))
    }

    async fn get_current_limit_breakdown(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Vec<OrganizationCreditLimit>> {
        let (mut limits, _, unattributed) = self.get_current_credit_status(organization_id).await?;

        // With one active credit type, legacy spend has only one current
        // capacity bucket to reduce. Fold it into that type so its breakdown
        // agrees with the aggregate remaining balance. With multiple types,
        // keep the legacy amount unattributed rather than inventing a split.
        if let [status] = limits.as_mut_slice() {
            status.consumed = status.consumed.saturating_add(unattributed);
            status.available = status
                .limit
                .spend_limit
                .saturating_sub(status.consumed)
                .max(0);
        }

        Ok(limits
            .into_iter()
            .map(|status| OrganizationCreditLimit {
                credit_type: status.limit.credit_type,
                source: status.limit.source,
                amount: status.limit.spend_limit,
                consumed: status.consumed,
                available: status.available,
                currency: status.limit.currency,
            })
            .collect())
    }
}
