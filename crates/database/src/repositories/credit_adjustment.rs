use crate::pool::DbPool;
use crate::repositories::credit_allocation::lock_organization_accounting;
use crate::repositories::utils::map_db_error;
use anyhow::Context;
use chrono::{DateTime, Utc};
use services::common::RepositoryError;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustedUsageKind {
    Inference,
    Service,
}

impl AdjustedUsageKind {
    const fn parent_ids(self, usage_id: Uuid) -> (Option<Uuid>, Option<Uuid>) {
        match self {
            Self::Inference => (Some(usage_id), None),
            Self::Service => (None, Some(usage_id)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditAdjustmentKind {
    Correction,
    Writeoff,
}

impl CreditAdjustmentKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Correction => "correction",
            Self::Writeoff => "writeoff",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CreateCreditAdjustment {
    pub organization_id: Uuid,
    pub usage_id: Uuid,
    pub usage_kind: AdjustedUsageKind,
    pub adjustment_kind: CreditAdjustmentKind,
    pub amount: i64,
    pub reason: String,
    pub idempotency_key: String,
    pub changed_by_user_id: Option<Uuid>,
    pub changed_by_user_email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditAllocationReversal {
    pub allocation_id: Uuid,
    pub credit_type: String,
    pub amount: i64,
}

#[derive(Debug, Clone)]
pub struct CreditAdjustment {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub usage_id: Uuid,
    pub usage_kind: AdjustedUsageKind,
    pub adjustment_kind: CreditAdjustmentKind,
    pub amount: i64,
    pub unfunded_amount_reversed: i64,
    pub reason: String,
    pub idempotency_key: String,
    pub changed_by_user_id: Option<Uuid>,
    pub changed_by_user_email: Option<String>,
    pub created_at: DateTime<Utc>,
    pub allocation_reversals: Vec<CreditAllocationReversal>,
}

#[derive(Debug, Clone)]
pub struct CreditAdjustmentRepository {
    pool: DbPool,
}

impl CreditAdjustmentRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn create(
        &self,
        request: &CreateCreditAdjustment,
    ) -> Result<CreditAdjustment, RepositoryError> {
        if request.amount <= 0 {
            return Err(RepositoryError::ValidationFailed(
                "adjustment amount must be positive".to_string(),
            ));
        }
        if request.reason.trim().is_empty() || request.idempotency_key.trim().is_empty() {
            return Err(RepositoryError::ValidationFailed(
                "adjustment reason and idempotency key are required".to_string(),
            ));
        }
        if request.idempotency_key.len() > 100 {
            return Err(RepositoryError::ValidationFailed(
                "adjustment idempotency key must not exceed 100 characters".to_string(),
            ));
        }

        let mut client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;
        let transaction = client.transaction().await.map_err(map_db_error)?;
        lock_organization_accounting(&transaction, request.organization_id).await?;

        if let Some(existing) = transaction
            .query_opt(
                "SELECT * FROM usage_credit_adjustments WHERE organization_id = $1 AND idempotency_key = $2",
                &[&request.organization_id, &request.idempotency_key],
            )
            .await
            .map_err(map_db_error)?
        {
            let result = load_adjustment(&*transaction, &existing).await?;
            if result.usage_id != request.usage_id
                || result.usage_kind != request.usage_kind
                || result.adjustment_kind != request.adjustment_kind
                || result.amount != request.amount
                || result.reason != request.reason
            {
                return Err(RepositoryError::ValidationFailed(
                    "adjustment idempotency key already exists with different data".to_string(),
                ));
            }
            transaction.commit().await.map_err(map_db_error)?;
            return Ok(result);
        }

        let (inference_usage_id, service_usage_id) =
            request.usage_kind.parent_ids(request.usage_id);
        let usage = transaction
            .query_opt(
                r#"
                SELECT total_cost, unfunded_amount
                FROM organization_usage_log
                WHERE $2::UUID IS NOT NULL AND id = $2 AND organization_id = $1
                UNION ALL
                SELECT total_cost, unfunded_amount
                FROM organization_service_usage_log
                WHERE $3::UUID IS NOT NULL AND id = $3 AND organization_id = $1
                "#,
                &[
                    &request.organization_id,
                    &inference_usage_id,
                    &service_usage_id,
                ],
            )
            .await
            .map_err(map_db_error)?
            .ok_or_else(|| RepositoryError::NotFound("usage charge not found".to_string()))?;
        let original_unfunded =
            usage
                .get::<_, Option<i64>>("unfunded_amount")
                .ok_or_else(|| {
                    RepositoryError::ValidationFailed(
                        "legacy usage with unknown funding cannot be adjusted".to_string(),
                    )
                })?;

        let prior = transaction
            .query_one(
                r#"
                SELECT COALESCE(SUM(amount), 0)::BIGINT AS amount,
                       COALESCE(SUM(unfunded_amount_reversed), 0)::BIGINT AS unfunded
                FROM usage_credit_adjustments
                WHERE ($1::UUID IS NOT NULL AND inference_usage_id = $1)
                   OR ($2::UUID IS NOT NULL AND service_usage_id = $2)
                "#,
                &[&inference_usage_id, &service_usage_id],
            )
            .await
            .map_err(map_db_error)?;
        let effective_cost = usage
            .get::<_, i64>("total_cost")
            .saturating_sub(prior.get::<_, i64>("amount"));
        let unresolved_unfunded = original_unfunded
            .saturating_sub(prior.get::<_, i64>("unfunded"))
            .max(0);
        if request.amount > effective_cost {
            return Err(RepositoryError::ValidationFailed(
                "adjustment exceeds the remaining effective usage cost".to_string(),
            ));
        }
        if request.adjustment_kind == CreditAdjustmentKind::Writeoff
            && request.amount > unresolved_unfunded
        {
            return Err(RepositoryError::ValidationFailed(
                "write-off exceeds unresolved unfunded usage".to_string(),
            ));
        }

        let unfunded_reversed = match request.adjustment_kind {
            CreditAdjustmentKind::Correction => request.amount.min(unresolved_unfunded),
            CreditAdjustmentKind::Writeoff => request.amount,
        };
        let adjustment = transaction
            .query_one(
                r#"
                INSERT INTO usage_credit_adjustments (
                    organization_id, inference_usage_id, service_usage_id, adjustment_type,
                    amount, unfunded_amount_reversed, reason, idempotency_key,
                    changed_by_user_id, changed_by_user_email
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                RETURNING *
                "#,
                &[
                    &request.organization_id,
                    &inference_usage_id,
                    &service_usage_id,
                    &request.adjustment_kind.as_str(),
                    &request.amount,
                    &unfunded_reversed,
                    &request.reason,
                    &request.idempotency_key,
                    &request.changed_by_user_id,
                    &request.changed_by_user_email,
                ],
            )
            .await
            .map_err(map_db_error)?;

        let mut funded_to_reverse = request.amount - unfunded_reversed;
        if funded_to_reverse > 0 {
            if request.adjustment_kind == CreditAdjustmentKind::Writeoff {
                return Err(RepositoryError::ValidationFailed(
                    "write-offs cannot reverse funded allocations".to_string(),
                ));
            }
            let allocations = transaction
                .query(
                    r#"
                    SELECT allocation.id, allocation.amount
                         - COALESCE(reversed.amount, 0)::BIGINT AS available
                    FROM usage_credit_allocations allocation
                    LEFT JOIN (
                        SELECT allocation_id, SUM(amount)::BIGINT AS amount
                        FROM usage_credit_allocation_reversals
                        GROUP BY allocation_id
                    ) reversed ON reversed.allocation_id = allocation.id
                    WHERE ($1::UUID IS NOT NULL AND allocation.inference_usage_id = $1)
                       OR ($2::UUID IS NOT NULL AND allocation.service_usage_id = $2)
                    ORDER BY allocation.priority_position DESC
                    "#,
                    &[&inference_usage_id, &service_usage_id],
                )
                .await
                .map_err(map_db_error)?;
            for allocation in allocations {
                if funded_to_reverse == 0 {
                    break;
                }
                let available = allocation.get::<_, i64>("available").max(0);
                let amount = funded_to_reverse.min(available);
                if amount == 0 {
                    continue;
                }
                transaction
                    .execute(
                        "INSERT INTO usage_credit_allocation_reversals (adjustment_id, allocation_id, amount) VALUES ($1, $2, $3)",
                        &[&adjustment.get::<_, Uuid>("id"), &allocation.get::<_, Uuid>("id"), &amount],
                    )
                    .await
                    .map_err(map_db_error)?;
                funded_to_reverse -= amount;
            }
            if funded_to_reverse != 0 {
                return Err(RepositoryError::ValidationFailed(
                    "adjustment does not reconcile with remaining funding".to_string(),
                ));
            }
        }

        let updated = transaction
            .execute(
                r#"UPDATE organization_balance
                   SET total_spent = total_spent - $2, updated_at = NOW()
                   WHERE organization_id = $1 AND total_spent >= $2"#,
                &[&request.organization_id, &request.amount],
            )
            .await
            .map_err(map_db_error)?;
        if updated != 1 {
            return Err(RepositoryError::ValidationFailed(
                "adjustment does not reconcile with organization balance".to_string(),
            ));
        }

        let result = load_adjustment(&*transaction, &adjustment).await?;
        transaction.commit().await.map_err(map_db_error)?;
        Ok(result)
    }
}

async fn load_adjustment<C: tokio_postgres::GenericClient + Sync>(
    client: &C,
    row: &tokio_postgres::Row,
) -> Result<CreditAdjustment, RepositoryError> {
    let adjustment_type: String = row.get("adjustment_type");
    let adjustment_kind = match adjustment_type.as_str() {
        "correction" => CreditAdjustmentKind::Correction,
        "writeoff" => CreditAdjustmentKind::Writeoff,
        _ => {
            return Err(RepositoryError::ValidationFailed(
                "stored adjustment type is invalid".to_string(),
            ));
        }
    };
    let inference_usage_id: Option<Uuid> = row.get("inference_usage_id");
    let service_usage_id: Option<Uuid> = row.get("service_usage_id");
    let (usage_kind, usage_id) = match (inference_usage_id, service_usage_id) {
        (Some(id), None) => (AdjustedUsageKind::Inference, id),
        (None, Some(id)) => (AdjustedUsageKind::Service, id),
        _ => {
            return Err(RepositoryError::ValidationFailed(
                "stored adjustment parent is invalid".to_string(),
            ));
        }
    };
    let reversals = client
        .query(
            r#"
            SELECT reversal.allocation_id, allocation.credit_type, reversal.amount
            FROM usage_credit_allocation_reversals reversal
            JOIN usage_credit_allocations allocation ON allocation.id = reversal.allocation_id
            WHERE reversal.adjustment_id = $1
            ORDER BY allocation.priority_position DESC
            "#,
            &[&row.get::<_, Uuid>("id")],
        )
        .await
        .map_err(map_db_error)?
        .into_iter()
        .map(|row| CreditAllocationReversal {
            allocation_id: row.get("allocation_id"),
            credit_type: row.get("credit_type"),
            amount: row.get("amount"),
        })
        .collect();
    Ok(CreditAdjustment {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        usage_id,
        usage_kind,
        adjustment_kind,
        amount: row.get("amount"),
        unfunded_amount_reversed: row.get("unfunded_amount_reversed"),
        reason: row.get("reason"),
        idempotency_key: row.get("idempotency_key"),
        changed_by_user_id: row.get("changed_by_user_id"),
        changed_by_user_email: row.get("changed_by_user_email"),
        created_at: row.get("created_at"),
        allocation_reversals: reversals,
    })
}
