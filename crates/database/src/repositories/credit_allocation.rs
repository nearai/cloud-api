use crate::repositories::utils::map_db_error;
use services::common::RepositoryError;
use services::usage::CreditAllocation;
use tokio_postgres::{GenericClient, Transaction};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditAllocationPolicy {
    pub priority: Vec<String>,
    pub version: String,
}

impl Default for CreditAllocationPolicy {
    fn default() -> Self {
        let config = config::CreditAllocationConfig::default();
        Self::from(&config)
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
            LEFT JOIN organization_credit_consumption consumed
              ON consumed.organization_id = active.organization_id
             AND consumed.credit_type = active.credit_type
            ORDER BY wanted.position
            "#,
            &[&organization_id, &policy.priority],
        )
        .await
        .map_err(map_db_error)?;

    // Pre-allocation usage has no defensible per-type split. The migration
    // snapshots it once so the posting hot path does not rescan lifetime usage
    // while holding the organization's accounting lock.
    let legacy_unattributed: i64 = transaction
        .query_one(
            r#"
            SELECT COALESCE((
                SELECT legacy_unattributed_amount
                FROM organization_balance
                WHERE organization_id = $1
            ), 0)::BIGINT AS amount
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
        transaction
            .execute(
                r#"
                INSERT INTO organization_credit_consumption (
                    organization_id, credit_type, amount, updated_at
                ) VALUES ($1, $2, $3, NOW())
                ON CONFLICT (organization_id, credit_type) DO UPDATE SET
                    amount = organization_credit_consumption.amount + EXCLUDED.amount,
                    updated_at = NOW()
                "#,
                &[&organization_id, &credit_type, &amount],
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

    let updated = transaction
        .execute(
            r#"UPDATE organization_balance
               SET unresolved_unfunded_amount = unresolved_unfunded_amount + $2,
                   updated_at = NOW()
               WHERE organization_id = $1"#,
            &[&organization_id, &remaining],
        )
        .await
        .map_err(map_db_error)?;
    if updated != 1 {
        return Err(RepositoryError::ValidationFailed(
            "organization accounting balance is missing".to_string(),
        ));
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
            SELECT allocation.credit_type,
                   allocation.amount - COALESCE((
                       SELECT SUM(reversal.amount)::BIGINT
                       FROM usage_credit_allocation_reversals reversal
                       WHERE reversal.allocation_id = allocation.id
                   ), 0) AS amount,
                   allocation.source, allocation.organization_limit_id,
                   allocation.policy_version
            FROM usage_credit_allocations allocation
            WHERE (($1::UUID IS NOT NULL AND allocation.inference_usage_id = $1)
                OR ($2::UUID IS NOT NULL AND allocation.service_usage_id = $2))
              AND allocation.amount > COALESCE((
                  SELECT SUM(reversal.amount)::BIGINT
                  FROM usage_credit_allocation_reversals reversal
                  WHERE reversal.allocation_id = allocation.id
              ), 0)
            ORDER BY allocation.priority_position
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
