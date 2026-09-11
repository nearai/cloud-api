use crate::repositories::utils::map_db_error;
use services::common::RepositoryError;
use services::usage::CreditAllocation;
use tokio_postgres::{GenericClient, Transaction};
use uuid::Uuid;

pub const DEFAULT_CREDIT_USAGE_ORDER: [&str; 4] = ["grant", "postpay", "staking_farm", "payment"];
pub const DEFAULT_ALLOCATION_POLICY_VERSION: &str = "v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditAllocationPolicy {
    pub priority: Vec<String>,
    pub version: String,
}

impl Default for CreditAllocationPolicy {
    fn default() -> Self {
        Self {
            priority: DEFAULT_CREDIT_USAGE_ORDER
                .into_iter()
                .map(str::to_string)
                .collect(),
            version: DEFAULT_ALLOCATION_POLICY_VERSION.to_string(),
        }
    }
}

impl From<&config::CreditAllocationConfig> for CreditAllocationPolicy {
    fn from(value: &config::CreditAllocationConfig) -> Self {
        Self {
            priority: value.priority.clone(),
            version: value.policy_version.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum UsageAllocationParent {
    Inference(Uuid),
    Service(Uuid),
}

#[derive(Debug, Clone)]
pub struct AllocationResult {
    pub allocations: Vec<CreditAllocation>,
    pub funded_amount: i64,
    pub unfunded_amount: i64,
}

/// Serialize all accounting writers for one organization on its durable row.
/// Limit changes and staking syncs take the same lock.
pub async fn lock_organization_accounting(
    transaction: &Transaction<'_>,
    organization_id: Uuid,
) -> Result<(), RepositoryError> {
    let row = transaction
        .query_opt(
            "SELECT id FROM organizations WHERE id = $1 AND is_active = true FOR UPDATE",
            &[&organization_id],
        )
        .await
        .map_err(map_db_error)?;
    if row.is_none() {
        return Err(RepositoryError::NotFound(format!(
            "Active organization not found: {organization_id}"
        )));
    }
    Ok(())
}

/// Allocate an already-priced charge against current per-type capacities.
/// Must run in the same transaction as the usage insert and balance update.
pub async fn allocate_usage(
    transaction: &Transaction<'_>,
    organization_id: Uuid,
    parent: UsageAllocationParent,
    total_cost: i64,
    policy: &CreditAllocationPolicy,
) -> Result<AllocationResult, RepositoryError> {
    if total_cost < 0 {
        return Err(RepositoryError::ValidationFailed(
            "usage cost must be non-negative".to_string(),
        ));
    }

    lock_organization_accounting(transaction, organization_id).await?;

    let rows = transaction
        .query(
            r#"
            SELECT active.id, active.credit_type, active.source, active.spend_limit,
                   COALESCE(consumed.amount, 0)::BIGINT AS consumed
            FROM unnest($2::TEXT[]) WITH ORDINALITY AS wanted(credit_type, position)
            JOIN organization_limits_history active
              ON active.organization_id = $1
             AND active.credit_type = wanted.credit_type
             AND active.effective_until IS NULL
            LEFT JOIN (
                SELECT allocation.credit_type,
                       (SUM(allocation.amount) - COALESCE(SUM(reversed.amount), 0))::BIGINT AS amount
                FROM usage_credit_allocations allocation
                LEFT JOIN (
                    SELECT allocation_id, SUM(amount)::BIGINT AS amount
                    FROM usage_credit_allocation_reversals
                    GROUP BY allocation_id
                ) reversed ON reversed.allocation_id = allocation.id
                WHERE allocation.organization_id = $1
                GROUP BY allocation.credit_type
            ) consumed ON consumed.credit_type = active.credit_type
            ORDER BY wanted.position
            "#,
            &[&organization_id, &policy.priority],
        )
        .await
        .map_err(map_db_error)?;

    // Pre-allocation usage has no defensible per-type split. Keep it unknown,
    // but reserve the same amount from aggregate capacity so rollout cannot
    // accidentally make already-spent credits available again.
    let legacy_unattributed: i64 = transaction
        .query_one(
            r#"
            SELECT GREATEST(
                COALESCE((SELECT total_spent FROM organization_balance
                          WHERE organization_id = $1), 0)
                - (
                    COALESCE((SELECT SUM(total_cost) FROM organization_usage_log
                              WHERE organization_id = $1 AND funded_amount IS NOT NULL), 0)
                  + COALESCE((SELECT SUM(total_cost) FROM organization_service_usage_log
                              WHERE organization_id = $1 AND funded_amount IS NOT NULL), 0)
                  - COALESCE((SELECT SUM(amount)::BIGINT FROM usage_credit_adjustments
                              WHERE organization_id = $1), 0)
                ),
                0
            )::BIGINT AS amount
            "#,
            &[&organization_id],
        )
        .await
        .map_err(map_db_error)?
        .get("amount");

    let mut aggregate_available = rows.iter().fold(0_i64, |total, row| {
        let limit: i64 = row.get("spend_limit");
        let consumed: i64 = row.get("consumed");
        total.saturating_add(limit.saturating_sub(consumed).max(0))
    });
    aggregate_available = aggregate_available
        .saturating_sub(legacy_unattributed)
        .max(0);

    let mut remaining = total_cost;
    let mut allocations = Vec::new();
    for (position, row) in rows.iter().enumerate() {
        if remaining == 0 || aggregate_available == 0 {
            break;
        }
        let limit: i64 = row.get("spend_limit");
        let consumed: i64 = row.get("consumed");
        let available = limit.saturating_sub(consumed).max(0);
        let amount = remaining.min(available).min(aggregate_available);
        if amount == 0 {
            continue;
        }

        let limit_id: Uuid = row.get("id");
        let credit_type: String = row.get("credit_type");
        let source: Option<String> = row.get("source");
        let (inference_usage_id, service_usage_id) = match parent {
            UsageAllocationParent::Inference(id) => (Some(id), None),
            UsageAllocationParent::Service(id) => (None, Some(id)),
        };
        let priority_position = i16::try_from(position).map_err(|_| {
            RepositoryError::ValidationFailed("credit priority is too long".to_string())
        })?;
        transaction
            .execute(
                r#"
                INSERT INTO usage_credit_allocations (
                    organization_id, inference_usage_id, service_usage_id,
                    credit_type, amount, organization_limit_id, source,
                    policy_version, priority_position
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                "#,
                &[
                    &organization_id,
                    &inference_usage_id,
                    &service_usage_id,
                    &credit_type,
                    &amount,
                    &limit_id,
                    &source,
                    &policy.version,
                    &priority_position,
                ],
            )
            .await
            .map_err(map_db_error)?;

        allocations.push(CreditAllocation {
            credit_type,
            amount,
            source,
            organization_limit_id: Some(limit_id),
            policy_version: policy.version.clone(),
        });
        remaining -= amount;
        aggregate_available -= amount;
    }

    Ok(AllocationResult {
        funded_amount: total_cost - remaining,
        unfunded_amount: remaining,
        allocations,
    })
}

pub async fn load_allocations<C: GenericClient + Sync>(
    client: &C,
    parent: UsageAllocationParent,
) -> Result<Vec<CreditAllocation>, RepositoryError> {
    let (inference_id, service_id) = match parent {
        UsageAllocationParent::Inference(id) => (Some(id), None),
        UsageAllocationParent::Service(id) => (None, Some(id)),
    };
    let rows = client
        .query(
            r#"
            SELECT credit_type, amount, source, organization_limit_id, policy_version
            FROM usage_credit_allocations
            WHERE ($1::UUID IS NOT NULL AND inference_usage_id = $1)
               OR ($2::UUID IS NOT NULL AND service_usage_id = $2)
            ORDER BY priority_position
            "#,
            &[&inference_id, &service_id],
        )
        .await
        .map_err(map_db_error)?;
    Ok(rows
        .into_iter()
        .map(|row| CreditAllocation {
            credit_type: row.get("credit_type"),
            amount: row.get("amount"),
            source: row.get("source"),
            organization_limit_id: row.get("organization_limit_id"),
            policy_version: row.get("policy_version"),
        })
        .collect())
}
