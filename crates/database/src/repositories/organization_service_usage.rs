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
use services::service_usage::ports::{ServiceUsageReportEntry, ServiceUsageReportFilters};
use std::time::Duration;
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
    reporting_statement_timeout: Duration,
    allocation_policy: CreditAllocationPolicy,
}

impl OrganizationServiceUsageRepository {
    pub fn new(pool: DbPool) -> Self {
        Self {
            pool,
            reporting_statement_timeout:
                crate::repositories::reporting_query::DEFAULT_REPORTING_STATEMENT_TIMEOUT,
            allocation_policy: CreditAllocationPolicy::default(),
        }
    }

    pub fn with_reporting_statement_timeout(pool: DbPool, statement_timeout: Duration) -> Self {
        Self {
            pool,
            reporting_statement_timeout: statement_timeout,
            allocation_policy: CreditAllocationPolicy::default(),
        }
    }

    pub fn with_accounting_config(
        pool: DbPool,
        statement_timeout: Duration,
        config: &config::CreditAllocationConfig,
    ) -> Self {
        Self {
            pool,
            reporting_statement_timeout: statement_timeout,
            allocation_policy: CreditAllocationPolicy::from(config),
        }
    }

    /// List service usage rows for an organization, optionally filtered by service_id.
    /// Results are ordered by created_at DESC.
    pub async fn list_for_org(
        &self,
        organization_id: Uuid,
        service_id: Option<Uuid>,
        credit_type: Option<&str>,
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
                        r#"SELECT COUNT(*)::BIGINT FROM organization_service_usage_log usage_log
                           WHERE organization_id = $1 AND service_id = $2
                             AND ($3::TEXT IS NULL OR EXISTS (
                                 SELECT 1 FROM usage_credit_allocations a
                                 WHERE a.service_usage_id = usage_log.id AND a.credit_type = $3
                                   AND a.amount > COALESCE((SELECT SUM(amount)::BIGINT
                                       FROM usage_credit_allocation_reversals reversal
                                       WHERE reversal.allocation_id = a.id), 0)))"#,
                        &[&organization_id, &service_id, &credit_type],
                    )
                    .await
                    .map_err(map_db_error)?
                    .get(0);

                let rows = client
                    .query(
                        r#"SELECT usage_log.id, usage_log.organization_id, usage_log.workspace_id,
                            usage_log.api_key_id, usage_log.service_id, usage_log.quantity,
                            CASE WHEN $3::TEXT IS NULL THEN usage_log.total_cost - COALESCE((
                                SELECT SUM(amount)::BIGINT FROM usage_credit_adjustments adjustment
                                WHERE adjustment.service_usage_id = usage_log.id
                            ), 0) ELSE
                                (SELECT a.amount - COALESCE((SELECT SUM(amount)::BIGINT
                                     FROM usage_credit_allocation_reversals reversal
                                     WHERE reversal.allocation_id = a.id), 0)
                                 FROM usage_credit_allocations a
                                 WHERE a.service_usage_id = usage_log.id AND a.credit_type = $3)
                            END AS total_cost,
                            usage_log.inference_id, usage_log.created_at,
                            usage_log.funded_amount, usage_log.unfunded_amount,
                            usage_log.allocation_policy_version,
                            CASE WHEN usage_log.funded_amount IS NULL THEN NULL ELSE
                                COALESCE((SELECT jsonb_agg(jsonb_build_object(
                                    'type', a.credit_type, 'amount', a.amount - COALESCE((SELECT SUM(amount)::BIGINT
                                        FROM usage_credit_allocation_reversals reversal
                                        WHERE reversal.allocation_id = a.id), 0), 'source', a.source,
                                    'organization_limit_id', a.organization_limit_id,
                                    'policy_version', a.policy_version
                                ) ORDER BY a.priority_position)
                                FROM usage_credit_allocations a
                                WHERE a.service_usage_id = usage_log.id
                                  AND ($3::TEXT IS NULL OR a.credit_type = $3)
                                  AND a.amount > COALESCE((SELECT SUM(amount)::BIGINT
                                      FROM usage_credit_allocation_reversals reversal
                                      WHERE reversal.allocation_id = a.id), 0)), '[]'::jsonb)
                            END AS credit_allocations
                           FROM organization_service_usage_log usage_log
                           WHERE organization_id = $1 AND service_id = $2
                             AND ($3::TEXT IS NULL OR EXISTS (
                                 SELECT 1 FROM usage_credit_allocations filter_allocation
                                 WHERE filter_allocation.service_usage_id = usage_log.id
                                   AND filter_allocation.credit_type = $3
                                   AND filter_allocation.amount > COALESCE((SELECT SUM(amount)::BIGINT
                                       FROM usage_credit_allocation_reversals reversal
                                       WHERE reversal.allocation_id = filter_allocation.id), 0)))
                           ORDER BY created_at DESC LIMIT $4 OFFSET $5"#,
                        &[&organization_id, &service_id, &credit_type, &limit, &offset],
                    )
                    .await
                    .map_err(map_db_error)?;

                Ok::<_, RepositoryError>((rows, total))
            } else {
                let total: i64 = client
                    .query_one(
                        r#"SELECT COUNT(*)::BIGINT FROM organization_service_usage_log usage_log
                           WHERE organization_id = $1
                             AND ($2::TEXT IS NULL OR EXISTS (
                                 SELECT 1 FROM usage_credit_allocations a
                                 WHERE a.service_usage_id = usage_log.id AND a.credit_type = $2
                                   AND a.amount > COALESCE((SELECT SUM(amount)::BIGINT
                                       FROM usage_credit_allocation_reversals reversal
                                       WHERE reversal.allocation_id = a.id), 0)))"#,
                        &[&organization_id, &credit_type],
                    )
                    .await
                    .map_err(map_db_error)?
                    .get(0);

                let rows = client
                    .query(
                        r#"SELECT usage_log.id, usage_log.organization_id, usage_log.workspace_id,
                            usage_log.api_key_id, usage_log.service_id, usage_log.quantity,
                            CASE WHEN $2::TEXT IS NULL THEN usage_log.total_cost - COALESCE((
                                SELECT SUM(amount)::BIGINT FROM usage_credit_adjustments adjustment
                                WHERE adjustment.service_usage_id = usage_log.id
                            ), 0) ELSE
                                (SELECT a.amount - COALESCE((SELECT SUM(amount)::BIGINT
                                     FROM usage_credit_allocation_reversals reversal
                                     WHERE reversal.allocation_id = a.id), 0)
                                 FROM usage_credit_allocations a
                                 WHERE a.service_usage_id = usage_log.id AND a.credit_type = $2)
                            END AS total_cost,
                            usage_log.inference_id, usage_log.created_at,
                            usage_log.funded_amount, usage_log.unfunded_amount,
                            usage_log.allocation_policy_version,
                            CASE WHEN usage_log.funded_amount IS NULL THEN NULL ELSE
                                COALESCE((SELECT jsonb_agg(jsonb_build_object(
                                    'type', a.credit_type, 'amount', a.amount - COALESCE((SELECT SUM(amount)::BIGINT
                                        FROM usage_credit_allocation_reversals reversal
                                        WHERE reversal.allocation_id = a.id), 0), 'source', a.source,
                                    'organization_limit_id', a.organization_limit_id,
                                    'policy_version', a.policy_version
                                ) ORDER BY a.priority_position)
                                FROM usage_credit_allocations a
                                WHERE a.service_usage_id = usage_log.id
                                  AND ($2::TEXT IS NULL OR a.credit_type = $2)
                                  AND a.amount > COALESCE((SELECT SUM(amount)::BIGINT
                                      FROM usage_credit_allocation_reversals reversal
                                      WHERE reversal.allocation_id = a.id), 0)), '[]'::jsonb)
                            END AS credit_allocations
                           FROM organization_service_usage_log usage_log
                           WHERE organization_id = $1
                             AND ($2::TEXT IS NULL OR EXISTS (
                                 SELECT 1 FROM usage_credit_allocations filter_allocation
                                 WHERE filter_allocation.service_usage_id = usage_log.id
                                   AND filter_allocation.credit_type = $2
                                   AND filter_allocation.amount > COALESCE((SELECT SUM(amount)::BIGINT
                                       FROM usage_credit_allocation_reversals reversal
                                       WHERE reversal.allocation_id = filter_allocation.id), 0)))
                           ORDER BY created_at DESC LIMIT $3 OFFSET $4"#,
                        &[&organization_id, &credit_type, &limit, &offset],
                    )
                    .await
                    .map_err(map_db_error)?;

                Ok::<_, RepositoryError>((rows, total))
            }
        })?;

        let logs = rows
            .iter()
            .map(|row| self.row_to_log(row, None))
            .collect::<Result<Vec<_>>>()?;
        Ok((logs, total))
    }

    pub async fn list_reporting_usage(
        &self,
        filters: &ServiceUsageReportFilters,
    ) -> Result<Vec<ServiceUsageReportEntry>> {
        if let (Some(start_time), Some(end_time)) = (filters.start_time, filters.end_time) {
            anyhow::ensure!(end_time >= start_time, "invalid reporting date range");
        }

        let cursor_created_at = filters.cursor.map(|cursor| cursor.created_at);
        let cursor_id = filters.cursor.map(|cursor| cursor.id);
        let deadline = crate::repositories::reporting_query::reporting_deadline(
            self.reporting_statement_timeout,
            filters.deadline,
        )?;
        let rows = retry_db!("list_service_usage_report", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let transaction = client
                .build_transaction()
                .read_only(true)
                .start()
                .await
                .map_err(map_db_error)?;
            crate::repositories::reporting_query::configure_reporting_transaction(
                &transaction,
                crate::repositories::reporting_query::remaining_statement_timeout(deadline)?,
            )
            .await?;
            let rows = transaction
                .query(
                    r#"
                    SELECT
                        usage_log.id, usage_log.organization_id, usage_log.workspace_id,
                        usage_log.api_key_id, usage_log.service_id, services.service_name,
                        usage_log.quantity,
                        CASE WHEN $7::TEXT IS NULL THEN usage_log.total_cost - COALESCE((
                            SELECT SUM(amount)::BIGINT FROM usage_credit_adjustments adjustment
                            WHERE adjustment.service_usage_id = usage_log.id
                        ), 0) ELSE
                            (SELECT a.amount - COALESCE((
                                SELECT SUM(amount)::BIGINT FROM usage_credit_allocation_reversals reversal
                                WHERE reversal.allocation_id = a.id
                             ), 0) FROM usage_credit_allocations a
                             WHERE a.service_usage_id = usage_log.id AND a.credit_type = $7)
                        END AS total_cost,
                        usage_log.inference_id, usage_log.created_at,
                        usage_log.funded_amount, usage_log.unfunded_amount,
                        usage_log.allocation_policy_version,
                        CASE WHEN usage_log.funded_amount IS NULL THEN NULL ELSE
                            COALESCE((SELECT jsonb_agg(jsonb_build_object(
                                'type', a.credit_type, 'amount', a.amount - COALESCE((SELECT SUM(amount)::BIGINT
                                    FROM usage_credit_allocation_reversals reversal
                                    WHERE reversal.allocation_id = a.id), 0), 'source', a.source,
                                'organization_limit_id', a.organization_limit_id,
                                'policy_version', a.policy_version
                            ) ORDER BY a.priority_position)
                            FROM usage_credit_allocations a
                            WHERE a.service_usage_id = usage_log.id
                              AND ($7::TEXT IS NULL OR a.credit_type = $7)
                              AND a.amount > COALESCE((SELECT SUM(amount)::BIGINT
                                  FROM usage_credit_allocation_reversals reversal
                                  WHERE reversal.allocation_id = a.id), 0)), '[]'::jsonb)
                        END AS credit_allocations
                    FROM organization_service_usage_log AS usage_log
                    INNER JOIN services ON services.id = usage_log.service_id
                    WHERE usage_log.organization_id = $1
                      AND ($2::TEXT IS NULL OR services.service_name = $2)
                      AND ($3::UUID IS NULL OR usage_log.workspace_id = $3)
                      AND ($4::UUID IS NULL OR usage_log.api_key_id = $4)
                      AND ($5::TIMESTAMPTZ IS NULL OR usage_log.created_at >= $5)
                      AND ($6::TIMESTAMPTZ IS NULL OR usage_log.created_at <= $6)
                      AND ($7::TEXT IS NULL OR EXISTS (
                          SELECT 1 FROM usage_credit_allocations allocation_filter
                          WHERE allocation_filter.service_usage_id = usage_log.id
                            AND allocation_filter.credit_type = $7
                            AND allocation_filter.amount > COALESCE((
                                SELECT SUM(amount)::BIGINT FROM usage_credit_allocation_reversals reversal
                                WHERE reversal.allocation_id = allocation_filter.id
                            ), 0)
                      ))
                      AND ($8::TIMESTAMPTZ IS NULL OR $9::UUID IS NULL
                           OR (usage_log.created_at, usage_log.id) < ($8, $9))
                    ORDER BY usage_log.created_at DESC, usage_log.id DESC
                    LIMIT $10
                    "#,
                    &[
                        &filters.organization_id,
                        &filters.service_name,
                        &filters.workspace_id,
                        &filters.api_key_id,
                        &filters.start_time,
                        &filters.end_time,
                        &filters.credit_type,
                        &cursor_created_at,
                        &cursor_id,
                        &filters.limit,
                    ],
                )
                .await
                .map_err(map_db_error)?;
            transaction.commit().await.map_err(map_db_error)?;
            Ok::<_, RepositoryError>(rows)
        })?;

        rows.iter().map(Self::row_to_report_entry).collect()
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
                            SELECT usage_log.*,
                                   usage_log.total_cost - COALESCE((
                                       SELECT SUM(amount)::BIGINT
                                       FROM usage_credit_adjustments adjustment
                                       WHERE adjustment.service_usage_id = usage_log.id
                                   ), 0) AS effective_total_cost
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
        let total_cost = row
            .try_get("effective_total_cost")
            .unwrap_or_else(|_| row.get("total_cost"));
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

    fn row_to_report_entry(row: &Row) -> Result<ServiceUsageReportEntry> {
        let credit_allocations = row
            .try_get::<_, Option<serde_json::Value>>("credit_allocations")?
            .map(serde_json::from_value)
            .transpose()?;
        let total_cost = row.get("total_cost");
        let (funded_amount, unfunded_amount) =
            effective_funding(row, &credit_allocations, total_cost);
        Ok(ServiceUsageReportEntry {
            id: row.get("id"),
            organization_id: row.get("organization_id"),
            workspace_id: row.get("workspace_id"),
            api_key_id: row.get("api_key_id"),
            service_id: row.get("service_id"),
            service_name: row.get("service_name"),
            quantity: row.get("quantity"),
            total_cost: row.get("total_cost"),
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
