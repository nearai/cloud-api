use crate::models::OrganizationServiceUsageLog;
use crate::pool::DbPool;
use crate::repositories::credit_allocation::{
    allocate_usage, load_allocations, lock_organization_accounting, CreditAllocationPolicy,
    UsageAllocationParent,
};
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
    pub(crate) pool: DbPool,
    allocation_policy: CreditAllocationPolicy,
}

impl OrganizationServiceUsageRepository {
    pub fn new(pool: DbPool) -> Self {
        Self {
            pool,
            allocation_policy: CreditAllocationPolicy::default(),
        }
    }

    pub fn with_accounting_config(pool: DbPool, config: &config::CreditAllocationConfig) -> Self {
        Self {
            pool,
            allocation_policy: CreditAllocationPolicy::from(config),
        }
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
            // Lock before the child INSERT to avoid two concurrent FK
            // KEY SHARE locks deadlocking when allocation requests FOR UPDATE.
            lock_organization_accounting(&transaction, request.organization_id).await?;

            let id = Uuid::new_v4();
            let now = Utc::now();

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
            let (row, allocations) = match maybe_row {
                Some(_r) => {
                    let allocation = allocate_usage(
                        &transaction,
                        request.organization_id,
                        UsageAllocationParent::Service(id),
                        request.total_cost,
                        &self.allocation_policy,
                    )
                    .await?;
                    let r = transaction
                        .query_one(
                            r#"UPDATE organization_service_usage_log
                               SET funded_amount = $2, unfunded_amount = $3,
                                   allocation_policy_version = $4
                               WHERE id = $1 RETURNING *"#,
                            &[
                                &id,
                                &allocation.funded_amount,
                                &allocation.unfunded_amount,
                                &self.allocation_policy.version,
                            ],
                        )
                        .await
                        .map_err(map_db_error)?;
                    transaction
                        .execute(
                            r#"
                            INSERT INTO organization_balance (
                                organization_id, total_spent, last_usage_at, total_requests, total_tokens, updated_at
                            ) VALUES ($1, $2, $3, 0, 0, $4)
                            ON CONFLICT (organization_id) DO UPDATE SET
                                total_spent = organization_balance.total_spent + $2,
                                last_usage_at = $3,
                                updated_at = $4
                            "#,
                            &[
                                &request.organization_id,
                                &request.total_cost,
                                &now,
                                &now,
                            ],
                        )
                        .await
                        .map_err(map_db_error)?;

                    transaction.commit().await.map_err(map_db_error)?;
                    (r, Some(allocation.allocations))
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
                            SELECT usage_log.*, NULL::JSONB AS credit_allocations
                            FROM organization_service_usage_log usage_log
                            WHERE usage_log.organization_id = $1
                              AND usage_log.inference_id = $2
                            "#,
                            &[&request.organization_id, &request.inference_id],
                        )
                        .await
                        .map_err(map_db_error)?;
                    let conflicts = existing.get::<_, Uuid>("workspace_id") != request.workspace_id
                        || existing.get::<_, Uuid>("api_key_id") != request.api_key_id
                        || existing.get::<_, Uuid>("service_id") != request.service_id
                        || existing.get::<_, i32>("quantity") != request.quantity
                        || existing.get::<_, i64>("total_cost") != request.total_cost;
                    if conflicts {
                        return Err(RepositoryError::ValidationFailed(
                            "service usage id already exists with different billable data"
                                .to_string(),
                        ));
                    }
                    let allocations = if existing
                        .try_get::<_, Option<i64>>("funded_amount")
                        .ok()
                        .flatten()
                        .is_some()
                    {
                        Some(
                            load_allocations(
                                &**client,
                                UsageAllocationParent::Service(existing.get("id")),
                            )
                            .await?,
                        )
                    } else {
                        None
                    };
                    (existing, allocations)
                }
            };

            Ok::<_, RepositoryError>((row, allocations))
        })?;

        self.row_to_log(&result.0, result.1)
    }

    fn row_to_log(
        &self,
        row: &Row,
        allocations_override: Option<Vec<services::usage::CreditAllocation>>,
    ) -> Result<OrganizationServiceUsageLog> {
        let credit_allocations = match allocations_override {
            Some(allocations) => Some(allocations),
            None => row
                .try_get::<_, Option<serde_json::Value>>("credit_allocations")?
                .map(serde_json::from_value)
                .transpose()?,
        };
        let total_cost = row.get("total_cost");
        let (funded_amount, unfunded_amount) =
            effective_funding(row, &credit_allocations, total_cost);
        Ok(OrganizationServiceUsageLog {
            id: row.get("id"),
            organization_id: row.get("organization_id"),
            workspace_id: row.get("workspace_id"),
            api_key_id: row.get("api_key_id"),
            service_id: row.get("service_id"),
            quantity: row.get("quantity"),
            total_cost,
            inference_id: row.get("inference_id"),
            created_at: row.get("created_at"),
            credit_allocations,
            funded_amount,
            unfunded_amount,
            allocation_policy_version: row.try_get("allocation_policy_version").ok().flatten(),
        })
    }
}

fn effective_funding(
    row: &Row,
    credit_allocations: &Option<Vec<services::usage::CreditAllocation>>,
    total_cost: i64,
) -> (Option<i64>, Option<i64>) {
    if row
        .try_get::<_, Option<i64>>("funded_amount")
        .ok()
        .flatten()
        .is_none()
    {
        return (None, None);
    }
    let funded = credit_allocations
        .as_ref()
        .map(|allocations| allocations.iter().map(|allocation| allocation.amount).sum())
        .unwrap_or(0);
    (Some(funded), Some(total_cost - funded))
}
