use crate::models::{OrganizationLimitsHistory, UpdateOrganizationLimitsDbRequest};
use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use chrono::Utc;
use services::common::RepositoryError;
use tokio_postgres::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct OrganizationLimitsRepository {
    pool: DbPool,
}

#[derive(Debug, Clone)]
pub struct CurrentCreditStatus {
    pub limit: OrganizationLimitsHistory,
    pub consumed: i64,
    pub available: i64,
}

impl OrganizationLimitsRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Update organization limits - closes previous active limit of the same type and creates new one
    pub async fn update_limits(
        &self,
        organization_id: Uuid,
        request: &UpdateOrganizationLimitsDbRequest,
    ) -> Result<OrganizationLimitsHistory> {
        let row = retry_db!("update_organization_limits", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let transaction = client.transaction().await.map_err(map_db_error)?;

            // Check if organization exists
            let org_exists = transaction
                .query_opt(
                    "SELECT 1 FROM organizations WHERE id = $1 AND is_active = true FOR UPDATE",
                    &[&organization_id],
                )
                .await
                .map_err(map_db_error)?;

            if org_exists.is_none() {
                return Err(RepositoryError::NotFound(format!(
                    "Organization not found: {organization_id}"
                )));
            }

            let now = Utc::now();

            // Close any existing active limits of the same credit_type (set effective_until to now)
            transaction
                .execute(
                    r#"
                    UPDATE organization_limits_history
                    SET effective_until = $1
                    WHERE organization_id = $2 AND credit_type = $3 AND effective_until IS NULL
                    "#,
                    &[&now, &organization_id, &request.credit_type],
                )
                .await
                .map_err(map_db_error)?;

            // Insert new limit record
            let row = transaction
                .query_one(
                    r#"
                    INSERT INTO organization_limits_history (
                        organization_id,
                        spend_limit,
                        credit_type,
                        source,
                        currency,
                        effective_from,
                        changed_by,
                        change_reason,
                        changed_by_user_id,
                        changed_by_user_email
                    ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                    RETURNING id, organization_id, spend_limit, credit_type, source, currency,
                              effective_from, effective_until,
                              changed_by, change_reason, changed_by_user_id, changed_by_user_email, created_at
                    "#,
                    &[
                        &organization_id,
                        &request.spend_limit,
                        &request.credit_type,
                        &request.source,
                        &request.currency,
                        &now,
                        &request.changed_by,
                        &request.change_reason,
                        &request.changed_by_user_id,
                        &request.changed_by_user_email,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            transaction.commit().await.map_err(map_db_error)?;

            Ok::<tokio_postgres::Row, RepositoryError>(row)
        })?;

        Ok(self.row_to_limits_history(&row))
    }

    /// Get all current active limits for an organization (one per credit type)
    pub async fn get_current_limits(
        &self,
        organization_id: Uuid,
    ) -> Result<Vec<OrganizationLimitsHistory>> {
        let rows = retry_db!("get_current_organization_limits", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    SELECT id, organization_id, spend_limit, credit_type, source, currency,
                           effective_from, effective_until,
                           changed_by, change_reason, changed_by_user_id, changed_by_user_email, created_at
                    FROM organization_limits_history
                    WHERE organization_id = $1 AND effective_until IS NULL
                    ORDER BY credit_type, effective_from DESC
                    "#,
                    &[&organization_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        let limits = rows
            .iter()
            .map(|row| self.row_to_limits_history(row))
            .collect();
        Ok(limits)
    }

    /// Return active type ceilings with lifetime attributed consumption, plus
    /// unresolved overage. Consumption intentionally survives limit-row
    /// replacement so raising a cumulative ceiling adds only new capacity.
    pub async fn get_current_credit_status(
        &self,
        organization_id: Uuid,
    ) -> Result<(Vec<CurrentCreditStatus>, i64, i64)> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;
        let rows = client
            .query(
                r#"
                SELECT olh.id, olh.organization_id, olh.spend_limit, olh.credit_type,
                       olh.source, olh.currency, olh.effective_from, olh.effective_until,
                       olh.changed_by, olh.change_reason, olh.changed_by_user_id,
                       olh.changed_by_user_email, olh.created_at,
                       COALESCE(consumed.amount, 0)::BIGINT AS consumed
                FROM organization_limits_history olh
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
                ) consumed ON consumed.credit_type = olh.credit_type
                WHERE olh.organization_id = $1 AND olh.effective_until IS NULL
                ORDER BY olh.credit_type, olh.effective_from DESC
                "#,
                &[&organization_id],
            )
            .await
            .map_err(map_db_error)?;
        let statuses = rows
            .iter()
            .map(|row| {
                let limit = self.row_to_limits_history(row);
                let consumed = row.get::<_, i64>("consumed");
                CurrentCreditStatus {
                    available: limit.spend_limit.saturating_sub(consumed).max(0),
                    limit,
                    consumed,
                }
            })
            .collect();
        let funding = client
            .query_one(
                r#"
                SELECT (
                    COALESCE((SELECT SUM(unfunded_amount) FROM organization_usage_log
                              WHERE organization_id = $1), 0)
                  + COALESCE((SELECT SUM(unfunded_amount) FROM organization_service_usage_log
                              WHERE organization_id = $1), 0)
                  - COALESCE((SELECT SUM(unfunded_amount_reversed)::BIGINT
                              FROM usage_credit_adjustments
                              WHERE organization_id = $1), 0)
                )::BIGINT AS unfunded,
                GREATEST(
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
                )::BIGINT AS unattributed
                "#,
                &[&organization_id],
            )
            .await
            .map_err(map_db_error)?;
        Ok((
            statuses,
            funding.get("unfunded"),
            funding.get("unattributed"),
        ))
    }

    /// Count limits history for an organization
    pub async fn count_limits_history(&self, organization_id: Uuid) -> Result<i64> {
        let row = retry_db!("count_organization_limits_history", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    "SELECT COUNT(*) FROM organization_limits_history WHERE organization_id = $1",
                    &[&organization_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.get("count"))
    }

    /// Get all limits history for an organization
    pub async fn get_limits_history(
        &self,
        organization_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OrganizationLimitsHistory>> {
        let rows = retry_db!("get_organization_limits_history", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    SELECT id, organization_id, spend_limit, credit_type, source, currency,
                           effective_from, effective_until,
                           changed_by, change_reason, changed_by_user_id, changed_by_user_email, created_at
                    FROM organization_limits_history
                    WHERE organization_id = $1
                    ORDER BY effective_from DESC
                    LIMIT $2 OFFSET $3
                    "#,
                    &[&organization_id, &limit, &offset],
                )
                .await
                .map_err(map_db_error)
        })?;

        let history = rows
            .into_iter()
            .map(|row| self.row_to_limits_history(&row))
            .collect();
        Ok(history)
    }

    /// Helper method to convert database row to OrganizationLimitsHistory
    fn row_to_limits_history(&self, row: &Row) -> OrganizationLimitsHistory {
        OrganizationLimitsHistory {
            id: row.get("id"),
            organization_id: row.get("organization_id"),
            spend_limit: row.get("spend_limit"),
            credit_type: row.get("credit_type"),
            source: row.get("source"),
            currency: row.get("currency"),
            effective_from: row.get("effective_from"),
            effective_until: row.get("effective_until"),
            changed_by: row.get("changed_by"),
            change_reason: row.get("change_reason"),
            changed_by_user_id: row.get("changed_by_user_id"),
            changed_by_user_email: row.get("changed_by_user_email"),
            created_at: row.get("created_at"),
        }
    }
}
