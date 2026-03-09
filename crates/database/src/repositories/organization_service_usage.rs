use crate::models::OrganizationServiceUsageLog;
use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use chrono::Utc;
use services::common::RepositoryError;
use tokio_postgres::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct RecordServiceUsageRequest {
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub service_id: Uuid,
    pub quantity: i32,
    pub total_cost: i64,
    pub inference_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct OrganizationServiceUsageRepository {
    pool: DbPool,
}

impl OrganizationServiceUsageRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// List service usage rows for an organization, optionally filtered by service_id.
    /// Results are ordered by created_at DESC.
    pub async fn list_for_org(
        &self,
        organization_id: Uuid,
        service_id: Option<Uuid>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<OrganizationServiceUsageLog>, i64)> {
        let (rows, total) = retry_db!("list_service_usage", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            if let Some(service_id) = service_id {
                let total: i64 = client
                    .query_one(
                        "SELECT COUNT(*)::BIGINT FROM organization_service_usage_log WHERE organization_id = $1 AND service_id = $2",
                        &[&organization_id, &service_id],
                    )
                    .await
                    .map_err(map_db_error)?
                    .get(0);

                let rows = client
                    .query(
                        "SELECT * FROM organization_service_usage_log WHERE organization_id = $1 AND service_id = $2 ORDER BY created_at DESC LIMIT $3 OFFSET $4",
                        &[&organization_id, &service_id, &limit, &offset],
                    )
                    .await
                    .map_err(map_db_error)?;

                Ok::<_, RepositoryError>((rows, total))
            } else {
                let total: i64 = client
                    .query_one(
                        "SELECT COUNT(*)::BIGINT FROM organization_service_usage_log WHERE organization_id = $1",
                        &[&organization_id],
                    )
                    .await
                    .map_err(map_db_error)?
                    .get(0);

                let rows = client
                    .query(
                        "SELECT * FROM organization_service_usage_log WHERE organization_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
                        &[&organization_id, &limit, &offset],
                    )
                    .await
                    .map_err(map_db_error)?;

                Ok::<_, RepositoryError>((rows, total))
            }
        })?;

        let logs = rows.iter().map(|row| self.row_to_log(row)).collect();
        Ok((logs, total))
    }

    /// Record service usage and update organization_balance. Idempotent when inference_id is set:
    /// duplicate (organization_id, inference_id) skips insert and balance update.
    pub async fn record_usage(
        &self,
        request: &RecordServiceUsageRequest,
    ) -> Result<OrganizationServiceUsageLog> {
        let result = retry_db!("record_service_usage", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let transaction = client.transaction().await.map_err(map_db_error)?;

            let id = Uuid::new_v4();
            let now = Utc::now();
            let quantity_i64 = i64::from(request.quantity);

            let maybe_row = transaction
                .query_opt(
                    r#"
                    INSERT INTO organization_service_usage_log (
                        id, organization_id, workspace_id, api_key_id, service_id,
                        quantity, total_cost, inference_id, created_at
                    ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                    ON CONFLICT (organization_id, inference_id) WHERE inference_id IS NOT NULL DO NOTHING
                    RETURNING *
                    "#,
                    &[
                        &id,
                        &request.organization_id,
                        &request.workspace_id,
                        &request.api_key_id,
                        &request.service_id,
                        &request.quantity,
                        &request.total_cost,
                        &request.inference_id,
                        &now,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            // When conflict occurs (duplicate org_id + inference_id), inference_id is always Some.
            // The partial unique index only applies when inference_id IS NOT NULL.
            let row = match maybe_row {
                Some(r) => {
                    transaction
                        .execute(
                            r#"
                            INSERT INTO organization_balance (
                                organization_id, total_spent, last_usage_at, total_requests, total_tokens, updated_at
                            ) VALUES ($1, $2, $3, $4, 0, $5)
                            ON CONFLICT (organization_id) DO UPDATE SET
                                total_spent = organization_balance.total_spent + $2,
                                total_requests = organization_balance.total_requests + $4,
                                last_usage_at = $3,
                                updated_at = $5
                            "#,
                            &[
                                &request.organization_id,
                                &request.total_cost,
                                &now,
                                &quantity_i64,
                                &now,
                            ],
                        )
                        .await
                        .map_err(map_db_error)?;

                    transaction.commit().await.map_err(map_db_error)?;
                    r
                }
                None => {
                    transaction.rollback().await.map_err(map_db_error)?;

                    // inference_id is Some here (conflict only when inference_id IS NOT NULL)
                    debug_assert!(
                        request.inference_id.is_some(),
                        "Conflict branch only reached when inference_id is set"
                    );
                    let existing = client
                        .query_one(
                            r#"
                            SELECT * FROM organization_service_usage_log
                            WHERE organization_id = $1 AND inference_id = $2
                            "#,
                            &[&request.organization_id, &request.inference_id],
                        )
                        .await
                        .map_err(map_db_error)?;
                    existing
                }
            };

            Ok::<_, RepositoryError>(row)
        })?;

        Ok(self.row_to_log(&result))
    }

    fn row_to_log(&self, row: &Row) -> OrganizationServiceUsageLog {
        OrganizationServiceUsageLog {
            id: row.get("id"),
            organization_id: row.get("organization_id"),
            workspace_id: row.get("workspace_id"),
            api_key_id: row.get("api_key_id"),
            service_id: row.get("service_id"),
            quantity: row.get("quantity"),
            total_cost: row.get("total_cost"),
            inference_id: row.get("inference_id"),
            created_at: row.get("created_at"),
        }
    }
}
