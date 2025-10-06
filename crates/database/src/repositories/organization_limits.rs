use crate::models::{OrganizationLimitsHistory, UpdateOrganizationLimitsDbRequest};
use crate::pool::DbPool;
use anyhow::{Context, Result};
use chrono::Utc;
use tokio_postgres::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct OrganizationLimitsRepository {
    pool: DbPool,
}

impl OrganizationLimitsRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Update organization limits - closes previous active limit and creates new one
    pub async fn update_limits(
        &self,
        organization_id: Uuid,
        request: &UpdateOrganizationLimitsDbRequest,
    ) -> Result<OrganizationLimitsHistory> {
        let mut client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let transaction = client
            .transaction()
            .await
            .context("Failed to start transaction")?;

        // Check if organization exists
        let org_exists = transaction
            .query_opt(
                "SELECT 1 FROM organizations WHERE id = $1 AND is_active = true",
                &[&organization_id],
            )
            .await
            .context("Failed to check if organization exists")?;

        if org_exists.is_none() {
            return Err(anyhow::anyhow!(
                "Organization not found: {}",
                organization_id
            ));
        }

        let now = Utc::now();

        // Close any existing active limits (set effective_until to now)
        transaction
            .execute(
                r#"
                UPDATE organization_limits_history
                SET effective_until = $1
                WHERE organization_id = $2 AND effective_until IS NULL
                "#,
                &[&now, &organization_id],
            )
            .await
            .context("Failed to close previous active limits")?;

        // Insert new limit record
        let row = transaction
            .query_one(
                r#"
                INSERT INTO organization_limits_history (
                    organization_id,
                    spend_limit,
                    effective_from,
                    changed_by,
                    change_reason
                ) VALUES ($1, $2, $3, $4, $5)
                RETURNING id, organization_id, spend_limit,
                          effective_from, effective_until,
                          changed_by, change_reason, created_at
                "#,
                &[
                    &organization_id,
                    &request.spend_limit,
                    &now,
                    &request.changed_by,
                    &request.change_reason,
                ],
            )
            .await
            .context("Failed to insert new organization limit")?;

        transaction
            .commit()
            .await
            .context("Failed to commit transaction")?;

        Ok(self.row_to_limits_history(&row))
    }

    /// Get current active limits for an organization
    pub async fn get_current_limits(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<OrganizationLimitsHistory>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
                SELECT id, organization_id, spend_limit,
                       effective_from, effective_until,
                       changed_by, change_reason, created_at
                FROM organization_limits_history
                WHERE organization_id = $1 AND effective_until IS NULL
                ORDER BY effective_from DESC
                LIMIT 1
                "#,
                &[&organization_id],
            )
            .await
            .context("Failed to query current organization limits")?;

        if let Some(row) = rows.first() {
            Ok(Some(self.row_to_limits_history(row)))
        } else {
            Ok(None)
        }
    }

    /// Get all limits history for an organization
    pub async fn get_limits_history(
        &self,
        organization_id: Uuid,
    ) -> Result<Vec<OrganizationLimitsHistory>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
                SELECT id, organization_id, spend_limit,
                       effective_from, effective_until,
                       changed_by, change_reason, created_at
                FROM organization_limits_history
                WHERE organization_id = $1
                ORDER BY effective_from DESC
                "#,
                &[&organization_id],
            )
            .await
            .context("Failed to query organization limits history")?;

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
            effective_from: row.get("effective_from"),
            effective_until: row.get("effective_until"),
            changed_by: row.get("changed_by"),
            change_reason: row.get("change_reason"),
            created_at: row.get("created_at"),
        }
    }
}
