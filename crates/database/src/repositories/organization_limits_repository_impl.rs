use crate::repositories::{organization_limits::CurrentCreditStatus, OrganizationLimitsRepository};
use services::usage::ports::{
    OrganizationCreditLimit, OrganizationCreditSnapshot, OrganizationLimit,
};
use uuid::Uuid;

fn aggregate_current_limit(
    limits: &[CurrentCreditStatus],
    unfunded: i64,
    unattributed: i64,
) -> Option<OrganizationLimit> {
    (!limits.is_empty()).then(|| OrganizationLimit {
        spend_limit: limits
            .iter()
            .map(|status| status.limit.spend_limit)
            .fold(0_i64, i64::saturating_add),
        available: limits
            .iter()
            .map(|status| status.available)
            .fold(0_i64, i64::saturating_add)
            .saturating_sub(unattributed)
            .max(0),
        unfunded,
    })
}

fn map_credit_breakdown(limits: Vec<CurrentCreditStatus>) -> Vec<OrganizationCreditLimit> {
    limits
        .into_iter()
        .map(|status| OrganizationCreditLimit {
            credit_type: status.limit.credit_type,
            source: status.limit.source,
            amount: status.limit.spend_limit,
            transferred_out: status.transferred_out,
            effective_capacity: status.effective_capacity,
            consumed: status.consumed,
            available: status.available,
            currency: status.limit.currency,
        })
        .collect()
}

/// Trait implementation adapter for OrganizationLimitsRepository
#[async_trait::async_trait]
impl services::usage::ports::OrganizationLimitsRepository for OrganizationLimitsRepository {
    async fn get_current_limits(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationLimit>> {
        let (limits, unfunded, unattributed) =
            self.get_current_credit_status(organization_id).await?;

        // All active types, including postpay, authorize usage. Postpay is a
        // contract safety ceiling rather than prepaid cash, but excluding it
        // here would leave a postpay-only organization unable to run requests.
        Ok(aggregate_current_limit(&limits, unfunded, unattributed))
    }

    async fn get_current_limit_breakdown(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Vec<OrganizationCreditLimit>> {
        let (limits, _, _) = self.get_current_credit_status(organization_id).await?;
        Ok(map_credit_breakdown(limits))
    }

    async fn get_current_credit_snapshot(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<OrganizationCreditSnapshot> {
        let (limits, unfunded, unattributed, version) = self
            .get_current_credit_status_with_version(organization_id)
            .await?;
        let limit = aggregate_current_limit(&limits, unfunded, unattributed);
        let breakdown = map_credit_breakdown(limits);
        Ok(OrganizationCreditSnapshot {
            limit,
            breakdown,
            version,
        })
    }
}
