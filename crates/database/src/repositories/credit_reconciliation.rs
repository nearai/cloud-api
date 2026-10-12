//! Reviewed reclassification of legacy consumption without rewriting usage.
use crate::pool::DbPool;
use crate::repositories::credit_allocation::lock_organization_accounting;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio_postgres::Transaction;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ReconciliationError {
    #[error("{0}")]
    Invalid(String),
    #[error("Organization not found or inactive")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreditPosition {
    /// Older snapshots contained only active types. New fields need serde defaults.
    #[serde(default = "active_snapshot_default")]
    pub active: bool,
    pub ceiling: i64,
    pub consumed: i64,
    pub available: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountingPosition {
    pub credits: BTreeMap<String, CreditPosition>,
    pub legacy_unattributed_amount: i64,
    pub unresolved_unfunded_amount: i64,
    pub aggregate_available: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReconciliationResult {
    /// Version of the persisted replay payload. Additive fields require serde defaults.
    #[serde(default = "snapshot_version")]
    pub schema_version: u32,
    pub reconciliation_id: Option<Uuid>,
    pub dry_run: bool,
    pub before: AccountingPosition,
    pub after: AccountingPosition,
}

fn active_snapshot_default() -> bool {
    true
}

fn snapshot_version() -> u32 {
    1
}

#[derive(Clone)]
pub struct CreditReconciliationRepository {
    pool: DbPool,
}

impl CreditReconciliationRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn reconcile(
        &self,
        organization_id: Uuid,
        amounts: &BTreeMap<String, i64>,
        idempotency_key: &str,
        reason: &str,
        created_by: Uuid,
        dry_run: bool,
    ) -> Result<ReconciliationResult> {
        let idempotency_key = idempotency_key.trim();
        let total = validate_request(amounts, idempotency_key, reason)?;
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.batch_execute("SET LOCAL statement_timeout = '5s'; SET LOCAL lock_timeout = '2s'")
            .await?;
        lock_organization_accounting(&tx, organization_id)
            .await
            .map_err(|error| match error {
                services::common::RepositoryError::NotFound(_) => {
                    anyhow::Error::from(ReconciliationError::NotFound)
                }
                services::common::RepositoryError::TransactionConflict => {
                    ReconciliationError::Conflict(
                        "Organization accounting is busy; retry the reconciliation".into(),
                    )
                    .into()
                }
                other => other.into(),
            })?;

        // Replay committed results before checking mutable accounting state.
        if let Some(row) = tx
            .query_opt(
                concat!(
                    "SELECT amounts, reason, result FROM organization_credit_reconciliations WHERE ",
                    "organization_id=$1 AND idempotency_key=$2",
                ),
                &[&organization_id, &idempotency_key],
            )
            .await?
        {
            let saved: BTreeMap<String, i64> = serde_json::from_value(row.get("amounts"))?;
            if saved != *amounts || row.get::<_, String>("reason") != reason {
                return Err(ReconciliationError::Conflict(
                    "Idempotency key was already used with different terms".into(),
                )
                .into());
            }
            // A preview must never be mistaken for a new uncommitted adjustment.
            if dry_run {
                return Err(ReconciliationError::Conflict(
    "This key already identifies a committed reconciliation; use a new key for a preview".into(),
).into());
            }
            let result: ReconciliationResult = serde_json::from_value(row.get("result"))?;
            anyhow::ensure!(
                result.schema_version == snapshot_version(),
                "Unsupported reconciliation snapshot version"
            );
            return Ok(result);
        }

        let active: bool = tx
            .query_one(
                "SELECT is_active FROM organizations WHERE id=$1",
                &[&organization_id],
            )
            .await?
            .get(0);
        if !active {
            return Err(ReconciliationError::NotFound.into());
        }

        // Historical discount corrections also reduce legacy spend and verify
        // counters against usage. Never reconcile while their worker is active.
        let applying: bool = tx
            .query_one(
                concat!(
                    "SELECT EXISTS (SELECT 1 FROM organization_usage_discounts WHERE organization_id=$1 ",
                    "AND status='applying')",
                ),
                &[&organization_id],
            )
            .await?
            .get(0);
        if applying {
            return Err(ReconciliationError::Conflict(
                "Wait for historical usage discount correction to finish before reconciling".into(),
            )
            .into());
        }

        let before = load_position(&tx, organization_id).await?;
        let after = preview(&before, amounts, total)?;
        let id = (!dry_run).then(Uuid::new_v4);
        let result = ReconciliationResult {
            schema_version: snapshot_version(),
            reconciliation_id: id,
            dry_run,
            before,
            after,
        };

        if dry_run {
            tx.rollback().await?;
            return Ok(result);
        }

        tx.execute(
            concat!(
                "INSERT INTO organization_credit_reconciliations (id, organization_id, ",
                "idempotency_key, amounts, reason, created_by, result) VALUES ($1,$2,$3,$4,$5,$6,$7)",
            ),
            &[
                &id,
                &organization_id,
                &idempotency_key,
                &serde_json::to_value(amounts)?,
                &reason,
                &created_by,
                &serde_json::to_value(&result)?,
            ],
        )
        .await?;

        for (credit_type, amount) in amounts {
            tx.execute(
                concat!(
                    "INSERT INTO organization_credit_consumption (organization_id, credit_type, amount) ",
                    "VALUES ($1,$2,$3) ON CONFLICT (organization_id, credit_type) DO UPDATE SET ",
                    "amount=organization_credit_consumption.amount+EXCLUDED.amount, ",
                    "updated_at=clock_timestamp()",
                ),
                &[&organization_id, &credit_type, &amount],
            )
            .await?;
        }

        let updated = tx
            .execute(
                concat!(
                    "UPDATE organization_balance SET ",
                    "legacy_unattributed_amount=legacy_unattributed_amount-$2, ",
                    "updated_at=clock_timestamp() WHERE organization_id=$1 AND legacy_unattributed_amount ",
                    ">= $2",
                ),
                &[&organization_id, &total],
            )
            .await?;
        anyhow::ensure!(updated == 1, "Legacy balance changed during reconciliation");
        anyhow::ensure!(
            load_position(&tx, organization_id).await? == result.after,
            "Reconciliation balances did not match preview"
        );

        tx.commit().await?;
        Ok(result)
    }
}

fn validate_request(amounts: &BTreeMap<String, i64>, key: &str, reason: &str) -> Result<i64> {
    let invalid = |message: &str| ReconciliationError::Invalid(message.into());
    if key.trim().is_empty()
        || key.chars().count() > 100
        || reason.trim().is_empty()
        || reason.chars().count() > 2000
    {
        return Err(invalid(
            "Require an idempotency key (1–100 characters) and reason (1–2000 characters)",
        )
        .into());
    }
    if amounts.is_empty() || amounts.len() > 4 {
        return Err(invalid("Specify one to four credit types").into());
    }

    amounts.iter().try_fold(0_i64, |sum, (kind, amount)| {
        if !matches!(kind.as_str(), "grant" | "payment" | "staking_farm" | "postpay") || *amount <= 0 {
            return Err(invalid("Amounts must be positive integer nano-USD for grant, payment, staking_farm or postpay").into());
        }
        sum.checked_add(*amount).ok_or_else(|| invalid("Reconciliation amount overflow").into())
    })
}

async fn load_position(tx: &Transaction<'_>, org: Uuid) -> Result<AccountingPosition> {
    let balance = tx
        .query_opt(
            concat!(
                "SELECT legacy_unattributed_amount, unresolved_unfunded_amount FROM ",
                "organization_balance WHERE organization_id=$1",
            ),
            &[&org],
        )
        .await?
        .ok_or_else(|| {
            ReconciliationError::Conflict("Organization accounting balance is missing".into())
        })?;
    let rows = tx
        .query(
        r#"WITH active AS (
            SELECT credit_type, spend_limit, currency FROM organization_limits_history
            WHERE organization_id=$1 AND effective_until IS NULL
        ), consumed AS (
            SELECT credit_type, amount FROM organization_credit_consumption WHERE organization_id=$1
        ) SELECT COALESCE(l.credit_type,c.credit_type) AS credit_type,
                 l.credit_type IS NOT NULL AS active, COALESCE(l.spend_limit,0)::BIGINT AS spend_limit,
                 l.currency, COALESCE(c.amount,0)::BIGINT AS consumed
          FROM active l FULL JOIN consumed c USING (credit_type)"#,
        &[&org],
    )
        .await?;

    let mut position = AccountingPosition {
        credits: BTreeMap::new(),
        legacy_unattributed_amount: balance.get("legacy_unattributed_amount"),
        unresolved_unfunded_amount: balance.get("unresolved_unfunded_amount"),
        aggregate_available: 0,
    };

    for row in rows {
        let active: bool = row.get("active");
        if let Some(currency) = row.get::<_, Option<String>>("currency") {
            if currency != "USD" {
                return Err(ReconciliationError::Conflict(format!(
                    "Only USD credit positions can be reconciled; {} is {}",
                    row.get::<_, String>("credit_type"),
                    currency
                ))
                .into());
            }
        }
        let ceiling: i64 = row.get("spend_limit");
        let consumed: i64 = row.get("consumed");
        position.credits.insert(
            row.get("credit_type"),
            CreditPosition {
                active,
                ceiling,
                consumed,
                available: if active {
                    ceiling.saturating_sub(consumed).max(0)
                } else {
                    0
                },
            },
        );
    }

    position.aggregate_available = aggregate_available(&position);
    Ok(position)
}

fn aggregate_available(position: &AccountingPosition) -> i64 {
    position
        .credits
        .values()
        .map(|credit| credit.available)
        .fold(0_i64, i64::saturating_add)
        .saturating_sub(position.legacy_unattributed_amount)
        .max(0)
}

fn preview(
    before: &AccountingPosition,
    amounts: &BTreeMap<String, i64>,
    total: i64,
) -> Result<AccountingPosition> {
    let conflict = |message: &str| ReconciliationError::Conflict(message.into());
    if total > before.legacy_unattributed_amount {
        return Err(conflict("Amount exceeds remaining legacy unattributed consumption").into());
    }

    let mut after = before.clone();
    for (kind, amount) in amounts {
        let credit = after.credits.get_mut(kind).ok_or_else(|| {
            conflict("Each selected credit type needs an active reviewed ceiling")
        })?;
        if !credit.active {
            return Err(
                conflict("Each selected credit type needs an active reviewed ceiling").into(),
            );
        }
        // Reclassification must remove the same capacity from the type as it
        // releases from the legacy deduction. Reject capped/disabled positions.
        if *amount > credit.available {
            return Err(conflict(
                "Amount exceeds the selected type's remaining ceiling; review its opening position first",
            )
            .into());
        }
        credit.consumed = credit
            .consumed
            .checked_add(*amount)
            .ok_or_else(|| conflict("Consumption overflow"))?;
        credit.available -= amount;
    }

    after.legacy_unattributed_amount -= total;
    after.aggregate_available = aggregate_available(&after);
    if after.aggregate_available != before.aggregate_available {
        return Err(conflict("Reconciliation would change aggregate available capacity").into());
    }
    Ok(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_snapshot_shape_remains_readable() {
        let original = serde_json::json!({
            "reconciliation_id": null,
            "dry_run": true,
            "before": {
                "credits": {
                    "payment": {"ceiling": 100, "consumed": 0, "available": 100},
                },
                "legacy_unattributed_amount": 80,
                "unresolved_unfunded_amount": 0,
                "aggregate_available": 20,
            },
            "after": {
                "credits": {
                    "payment": {"ceiling": 100, "consumed": 80, "available": 20},
                },
                "legacy_unattributed_amount": 0,
                "unresolved_unfunded_amount": 0,
                "aggregate_available": 20,
            },
        });

        let result: ReconciliationResult = serde_json::from_value(original).unwrap();
        assert_eq!(result.schema_version, 1);
        assert!(result.before.credits["payment"].active);
        assert!(result.after.credits["payment"].active);
    }

    #[test]
    fn too_many_types_reports_the_actual_constraint() {
        let amounts = ["grant", "payment", "staking_farm", "postpay", "extra"]
            .into_iter()
            .map(|key| (key.to_string(), 1))
            .collect();
        assert_eq!(
            validate_request(&amounts, "key", "reason")
                .unwrap_err()
                .to_string(),
            "Specify one to four credit types"
        );
    }
}
