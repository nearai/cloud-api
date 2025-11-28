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
                    "SELECT 1 FROM organizations WHERE id = $1 AND is_active = true",
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
                .map_err(map_db_error)?;

            // Insert new limit record
            let row = transaction
                .query_one(
                    r#"
                    INSERT INTO organization_limits_history (
                        organization_id,
                        spend_limit,
                        effective_from,
                        changed_by,
                        change_reason,
                        changed_by_user_id,
                        changed_by_user_email
                    ) VALUES ($1, $2, $3, $4, $5, $6, $7)
                    RETURNING id, organization_id, spend_limit,
                              effective_from, effective_until,
                              changed_by, change_reason, changed_by_user_id, changed_by_user_email, created_at
                    "#,
                    &[
                        &organization_id,
                        &request.spend_limit,
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

    /// Get current active limits for an organization
    pub async fn get_current_limits(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<OrganizationLimitsHistory>> {
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
                    SELECT id, organization_id, spend_limit,
                           effective_from, effective_until,
                           changed_by, change_reason, changed_by_user_id, changed_by_user_email, created_at
                    FROM organization_limits_history
                    WHERE organization_id = $1 AND effective_until IS NULL
                    ORDER BY effective_from DESC
                    LIMIT 1
                    "#,
                    &[&organization_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        if let Some(row) = rows.first() {
            Ok(Some(self.row_to_limits_history(row)))
        } else {
            Ok(None)
        }
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
                    SELECT id, organization_id, spend_limit,
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
