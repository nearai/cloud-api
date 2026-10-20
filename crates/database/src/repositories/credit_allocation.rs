use crate::repositories::utils::map_db_error;
use services::common::RepositoryError;
use services::usage::CreditAllocation;
use services::usage::ServedProviderTier;
use tokio_postgres::{GenericClient, Transaction};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditAllocationPolicy {
    pub priority: Vec<String>,
    pub version: String,
    /// Credit types restricted to NEAR-tier (`near`) served inference. See
    /// `config::CreditAllocationConfig::near_tier_only_credit_types`.
    pub near_tier_only: Vec<String>,
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
            near_tier_only: value.near_tier_only_credit_types.clone(),
        }
    }
}

impl CreditAllocationPolicy {
    /// Funding priority for a charge served at the given provider tier.
    ///
    /// Credit types marked NEAR-tier-only are eligible only when the charge
    /// was served by NEAR AI's own fleet. Charges with any other tier — or
    /// with no tier attribution at all (legacy rows, unattributed reporters)
    /// — fail closed: restricted types are dropped from the candidate set so
    /// gifted credits can never fund third-party provider usage.
    pub fn priority_for_tier(
        &self,
        served_provider_tier: Option<ServedProviderTier>,
    ) -> Vec<String> {
        if served_provider_tier == Some(ServedProviderTier::Near) || self.near_tier_only.is_empty()
        {
            return self.priority.clone();
        }
        self.priority
            .iter()
            .filter(|credit_type| !self.near_tier_only.contains(credit_type))
            .cloned()
            .collect()
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
            // Accounting must still finish for an in-flight request when an
            // organization is deactivated after admission. The authorization
            // path owns the active-state check; this lock only establishes
            // that the durable accounting parent exists.
            "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
            &[&organization_id],
        )
        .await
        .map_err(map_db_error)?;
    if row.is_none() {
        return Err(RepositoryError::NotFound(format!(
            "Organization not found: {organization_id}"
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
                    policy_version, allocation_phase, priority_position
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'posting', $9)
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

/// Apply newly available credit capacity to previously unfunded usage.
///
/// Existing posting-time allocations are never updated. Each settlement is a
/// new immutable allocation row linked to the original usage, and the
/// organization accounting lock makes retries and concurrent credit writers
/// safe. Oldest overages are settled first for deterministic reconciliation.
pub async fn settle_unfunded_usage(
    transaction: &Transaction<'_>,
    organization_id: Uuid,
    policy: &CreditAllocationPolicy,
) -> Result<i64, RepositoryError> {
    lock_organization_accounting(transaction, organization_id).await?;

    let unresolved: i64 = transaction
        .query_one(
            r#"
            SELECT COALESCE((SELECT unresolved_unfunded_amount
                FROM organization_balance WHERE organization_id = $1), 0)::BIGINT AS amount
            "#,
            &[&organization_id],
        )
        .await
        .map_err(map_db_error)?
        .get("amount");
    if unresolved == 0 {
        return Ok(0);
    }

    let credit_rows = transaction
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

    let legacy_unattributed: i64 = transaction
        .query_one(
            r#"
            SELECT COALESCE((SELECT legacy_unattributed_amount
                FROM organization_balance WHERE organization_id = $1), 0)::BIGINT AS amount
            "#,
            &[&organization_id],
        )
        .await
        .map_err(map_db_error)?
        .get("amount");

    struct AvailableCredit {
        limit_id: Uuid,
        credit_type: String,
        source: Option<String>,
        amount: i64,
        priority_position: i16,
    }

    let mut credits = credit_rows
        .iter()
        .enumerate()
        .map(|(position, row)| {
            Ok(AvailableCredit {
                limit_id: row.get("id"),
                credit_type: row.get("credit_type"),
                source: row.get("source"),
                amount: row
                    .get::<_, i64>("spend_limit")
                    .saturating_sub(row.get::<_, i64>("consumed"))
                    .max(0),
                priority_position: i16::try_from(position).map_err(|_| {
                    RepositoryError::ValidationFailed("credit priority is too long".to_string())
                })?,
            })
        })
        .collect::<Result<Vec<_>, RepositoryError>>()?;
    let mut aggregate_available = credits
        .iter()
        .map(|credit| credit.amount)
        .fold(0_i64, i64::saturating_add)
        .saturating_sub(legacy_unattributed)
        .max(0);
    if aggregate_available == 0 {
        return Ok(0);
    }

    let debts = transaction
        .query(
            r#"
            SELECT usage_kind, usage_id, outstanding, served_provider_tier
            FROM (
                SELECT 'inference'::TEXT AS usage_kind, usage.id AS usage_id,
                       usage.created_at,
                       usage.served_provider_tier,
                       GREATEST(usage.unfunded_amount
                           - COALESCE((SELECT SUM(allocation.amount)::BIGINT
                               FROM effective_usage_credit_allocations allocation
                               WHERE allocation.inference_usage_id = usage.id
                                 AND allocation.allocation_phase = 'overage_settlement'), 0), 0)::BIGINT
                           AS outstanding
                FROM organization_usage_log usage
                WHERE usage.organization_id = $1 AND usage.unfunded_amount > 0
                UNION ALL
                SELECT 'service'::TEXT AS usage_kind, usage.id AS usage_id,
                       usage.created_at,
                       NULL::TEXT AS served_provider_tier,
                       GREATEST(usage.unfunded_amount
                           - COALESCE((SELECT SUM(allocation.amount)::BIGINT
                               FROM effective_usage_credit_allocations allocation
                               WHERE allocation.service_usage_id = usage.id
                                 AND allocation.allocation_phase = 'overage_settlement'), 0), 0)::BIGINT
                           AS outstanding
                FROM organization_service_usage_log usage
                WHERE usage.organization_id = $1 AND usage.unfunded_amount > 0
            ) debt
            WHERE outstanding > 0
            ORDER BY created_at, usage_id
            "#,
            &[&organization_id],
        )
        .await
        .map_err(map_db_error)?;

    let mut remaining_debt = unresolved;
    let mut settled = 0_i64;
    for debt in debts {
        if remaining_debt == 0 || aggregate_available == 0 {
            break;
        }
        let usage_kind: String = debt.get("usage_kind");
        let usage_id: Uuid = debt.get("usage_id");
        let debt_tier: Option<String> = debt.get("served_provider_tier");
        let debt_tier = debt_tier.and_then(|tier| tier.parse::<ServedProviderTier>().ok());
        // Settlement must obey the same tier scoping as posting: a charge that
        // was not served by the NEAR fleet can never draw on NEAR-tier-only
        // credit. Unattributed inference debts (legacy rows) fail closed.
        let allowed_types = match usage_kind.as_str() {
            "inference" => policy.priority_for_tier(debt_tier),
            // Platform services carry no provider tier and keep the full
            // funding priority.
            _ => policy.priority.clone(),
        };
        let mut outstanding = debt.get::<_, i64>("outstanding").min(remaining_debt);
        for credit in &mut credits {
            if outstanding == 0 || aggregate_available == 0 {
                break;
            }
            if !allowed_types.contains(&credit.credit_type) {
                continue;
            }
            let amount = outstanding.min(credit.amount).min(aggregate_available);
            if amount == 0 {
                continue;
            }
            let inference_usage_id = (usage_kind == "inference").then_some(usage_id);
            let service_usage_id = (usage_kind == "service").then_some(usage_id);
            transaction
                .execute(
                    r#"
                    INSERT INTO usage_credit_allocations (
                        organization_id, inference_usage_id, service_usage_id,
                        credit_type, amount, organization_limit_id, source,
                        policy_version, allocation_phase, priority_position
                    ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
                              'overage_settlement', $9)
                    "#,
                    &[
                        &organization_id,
                        &inference_usage_id,
                        &service_usage_id,
                        &credit.credit_type,
                        &amount,
                        &credit.limit_id,
                        &credit.source,
                        &policy.version,
                        &credit.priority_position,
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
                    &[&organization_id, &credit.credit_type, &amount],
                )
                .await
                .map_err(map_db_error)?;

            credit.amount -= amount;
            outstanding -= amount;
            aggregate_available -= amount;
            remaining_debt -= amount;
            settled += amount;
        }
    }

    if settled > 0 {
        let updated = transaction
            .execute(
                r#"
                UPDATE organization_balance
                SET unresolved_unfunded_amount = unresolved_unfunded_amount - $2,
                    updated_at = NOW()
                WHERE organization_id = $1 AND unresolved_unfunded_amount >= $2
                "#,
                &[&organization_id, &settled],
            )
            .await
            .map_err(map_db_error)?;
        if updated != 1 {
            return Err(RepositoryError::ValidationFailed(
                "overage settlement does not reconcile with organization balance".to_string(),
            ));
        }
    }

    Ok(settled)
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
                   allocation.amount AS amount,
                   allocation.source, allocation.organization_limit_id,
                   allocation.policy_version
            FROM effective_usage_credit_allocations allocation
            WHERE (($1::UUID IS NOT NULL AND allocation.inference_usage_id = $1)
                OR ($2::UUID IS NOT NULL AND allocation.service_usage_id = $2))
            ORDER BY allocation.created_at, allocation.priority_position, allocation.id
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

#[cfg(test)]
mod tier_scoping_tests {
    use super::*;

    fn policy() -> CreditAllocationPolicy {
        CreditAllocationPolicy {
            priority: ["grant", "staking_farm", "payment", "postpay"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            version: "test".to_string(),
            near_tier_only: vec!["grant".to_string()],
        }
    }

    #[test]
    fn near_tier_keeps_full_priority() {
        assert_eq!(
            policy().priority_for_tier(Some(ServedProviderTier::Near)),
            vec!["grant", "staking_farm", "payment", "postpay"]
        );
    }

    #[test]
    fn non_near_tiers_and_unattributed_fail_closed() {
        for tier in [
            None,
            Some(ServedProviderTier::Attested3p),
            Some(ServedProviderTier::NonAttested),
        ] {
            assert_eq!(
                policy().priority_for_tier(tier),
                vec!["staking_farm", "payment", "postpay"],
                "tier {tier:?} must not draw on NEAR-tier-only credit"
            );
        }
    }

    #[test]
    fn empty_scope_keeps_full_priority() {
        let mut unrestricted = policy();
        unrestricted.near_tier_only.clear();
        assert_eq!(
            unrestricted.priority_for_tier(None),
            vec!["grant", "staking_farm", "payment", "postpay"]
        );
    }
}
