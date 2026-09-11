use crate::models::{
    OrganizationBalance, OrganizationUsageLog, RecordUsageRequest, ServedProviderTier,
    ServedProviderType, StopReason,
};
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
use services::responses::models::ResponseId;
use std::collections::HashMap;
use std::time::Duration;
use tokio_postgres::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct OrganizationUsageRepository {
    pub(crate) pool: DbPool,
    pub(crate) reporting_statement_timeout: Duration,
    allocation_policy: CreditAllocationPolicy,
}

impl OrganizationUsageRepository {
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

    /// Get total spend for a specific API key
    pub async fn get_api_key_spend(&self, api_key_id: Uuid) -> Result<i64> {
        let row = retry_db!("get_api_key_spend", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    SELECT COALESCE(SUM(
                        usage_log.total_cost - COALESCE((
                            SELECT SUM(amount)::BIGINT
                            FROM usage_credit_adjustments adjustment
                            WHERE adjustment.inference_usage_id = usage_log.id
                        ), 0)
                    ), 0)::BIGINT AS total_spend
                    FROM organization_usage_log usage_log
                    WHERE usage_log.api_key_id = $1
                    "#,
                    &[&api_key_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        let total_spend: i64 = row.get("total_spend");
        Ok(total_spend)
    }

    /// Record usage and update balance atomically.
    ///
    /// When `inference_id` is set, this is idempotent: duplicate inserts for the
    /// same `(organization_id, inference_id)` skip the INSERT and balance update,
    /// returning the existing record instead.
    pub async fn record_usage(&self, request: RecordUsageRequest) -> Result<OrganizationUsageLog> {
        let result = retry_db!("record_organization_usage", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let transaction = client.transaction().await.map_err(map_db_error)?;
            // Take the exclusive accounting lock before inserting the child
            // usage row. Otherwise concurrent inserts first acquire FK
            // KEY SHARE locks and can deadlock when allocation upgrades them.
            lock_organization_accounting(&transaction, request.organization_id).await?;

            let id = Uuid::new_v4();
            let now = Utc::now();
            let total_tokens = request.input_tokens + request.output_tokens;

            // Insert usage log entry (model_name is denormalized for performance).
            // ON CONFLICT DO NOTHING: if inference_id already exists for this org,
            // the INSERT is skipped (no row returned) and we fetch the existing record.
            let stop_reason_str = request.stop_reason.as_ref().map(|r| r.as_str());
            let response_id_uuid = request.response_id.as_ref().map(|r| r.as_uuid());
            let served_provider_tier = request.served_provider_tier.map(|tier| tier.as_str());
            let served_provider_type = request
                .served_provider_type
                .map(|provider| provider.as_str());
            let maybe_row = transaction
                .query_opt(
                    r#"
                    INSERT INTO organization_usage_log (
                        id, organization_id, workspace_id, api_key_id,
                        model_id, model_name, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, total_tokens,
                        input_cost, output_cost, total_cost,
                        inference_type, created_at, ttft_ms, avg_itl_ms, inference_id,
                        provider_request_id, stop_reason, response_id, image_count,
                        served_provider_tier, served_provider_type, served_via_fallback,
                        billing_details, service_tier, context_band
                    ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29)
                    ON CONFLICT (organization_id, inference_id) WHERE inference_id IS NOT NULL DO NOTHING
                    RETURNING *
                    "#,
                    &[
                        &id,
                        &request.organization_id,
                        &request.workspace_id,
                        &request.api_key_id,
                        &request.model_id,
                        &request.model_name,
                        &request.input_tokens,
                        &request.output_tokens,
                        &request.cache_read_tokens,
                        &request.cache_write_tokens,
                        &total_tokens,
                        &request.input_cost,
                        &request.output_cost,
                        &request.total_cost,
                        &request.inference_type,
                        &now,
                        &request.ttft_ms,
                        &request.avg_itl_ms,
                        &request.inference_id,
                        &request.provider_request_id,
                        &stop_reason_str,
                        &response_id_uuid,
                        &request.image_count,
                        &served_provider_tier,
                        &served_provider_type,
                        &request.served_via_fallback,
                        &request.billing_details,
                        &request.service_tier,
                        &request.context_band,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            let (row, was_inserted, allocations) = match maybe_row {
                Some(_row) => {
                    let allocation = allocate_usage(
                        &transaction,
                        request.organization_id,
                        UsageAllocationParent::Inference(id),
                        request.total_cost,
                        &self.allocation_policy,
                    )
                    .await?;
                    let row = transaction
                        .query_one(
                            r#"
                            UPDATE organization_usage_log
                            SET funded_amount = $2, unfunded_amount = $3,
                                allocation_policy_version = $4
                            WHERE id = $1
                            RETURNING *
                            "#,
                            &[
                                &id,
                                &allocation.funded_amount,
                                &allocation.unfunded_amount,
                                &self.allocation_policy.version,
                            ],
                        )
                        .await
                        .map_err(map_db_error)?;
                    // New insert succeeded — update organization balance
                    transaction
                        .execute(
                            r#"
                            INSERT INTO organization_balance (
                                organization_id,
                                total_spent,
                                last_usage_at,
                                total_requests,
                                total_tokens,
                                updated_at
                            ) VALUES ($1, $2, $3, 1, $4, $5)
                            ON CONFLICT (organization_id) DO UPDATE SET
                                total_spent = organization_balance.total_spent + $2,
                                total_requests = organization_balance.total_requests + 1,
                                total_tokens = organization_balance.total_tokens + $4,
                                last_usage_at = $3,
                                updated_at = $5
                            "#,
                            &[
                                &request.organization_id,
                                &request.total_cost,
                                &now,
                                &(total_tokens as i64),
                                &now,
                            ],
                        )
                        .await
                        .map_err(map_db_error)?;

                    transaction.commit().await.map_err(map_db_error)?;
                    (row, true, Some(allocation.allocations))
                }
                None => {
                    // Duplicate — inference_id already exists for this org.
                    // Roll back (nothing was written) and fetch the existing record.
                    transaction.rollback().await.map_err(map_db_error)?;

                    tracing::debug!(
                        organization_id = %request.organization_id,
                        "Duplicate usage recording detected, returning existing record"
                    );

                    let existing = client
                        .query_one(
                            r#"
                            SELECT usage_log.*,
                                   usage_log.total_cost - COALESCE((
                                       SELECT SUM(amount)::BIGINT
                                       FROM usage_credit_adjustments adjustment
                                       WHERE adjustment.inference_usage_id = usage_log.id
                                   ), 0) AS filtered_total_cost
                            FROM organization_usage_log usage_log
                            WHERE usage_log.organization_id = $1
                              AND usage_log.inference_id = $2
                            "#,
                            &[&request.organization_id, &request.inference_id],
                        )
                        .await
                        .map_err(map_db_error)?;
                    let conflicts = existing.get::<_, Uuid>("workspace_id") != request.workspace_id
                        || existing.get::<_, Uuid>("api_key_id") != request.api_key_id
                        || existing.get::<_, Uuid>("model_id") != request.model_id
                        || existing.get::<_, String>("model_name") != request.model_name
                        || existing.get::<_, i32>("input_tokens") != request.input_tokens
                        || existing.get::<_, i32>("output_tokens") != request.output_tokens
                        || existing.get::<_, i32>("cache_read_tokens") != request.cache_read_tokens
                        || existing.get::<_, i32>("cache_write_tokens")
                            != request.cache_write_tokens
                        || existing.get::<_, i64>("input_cost") != request.input_cost
                        || existing.get::<_, i64>("output_cost") != request.output_cost
                        || existing.get::<_, i64>("total_cost") != request.total_cost
                        || existing
                            .try_get::<_, Option<String>>("inference_type")
                            .ok()
                            .flatten()
                            .as_deref()
                            != Some(request.inference_type.as_str())
                        || existing.get::<_, Option<i32>>("image_count") != request.image_count
                        || existing.get::<_, Option<serde_json::Value>>("billing_details")
                            != request.billing_details
                        || existing.get::<_, Option<String>>("service_tier")
                            != request.service_tier
                        || existing.get::<_, Option<String>>("context_band")
                            != request.context_band;
                    if conflicts {
                        return Err(RepositoryError::ValidationFailed(
                            "usage id already exists with different billable data".to_string(),
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
                                UsageAllocationParent::Inference(existing.get("id")),
                            )
                            .await?,
                        )
                    } else {
                        None
                    };
                    (existing, false, allocations)
                }
            };

            Ok::<
                (
                    tokio_postgres::Row,
                    bool,
                    Option<Vec<services::usage::CreditAllocation>>,
                ),
                RepositoryError,
            >((row, was_inserted, allocations))
        })?;

        let (row, was_inserted, allocations) = result;
        self.row_to_usage_log(&row, was_inserted, allocations)
    }

    /// Get current balance for an organization
    pub async fn get_balance(&self, organization_id: Uuid) -> Result<Option<OrganizationBalance>> {
        let row_opt = retry_db!("get_organization_balance", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    SELECT organization_id, total_spent, last_usage_at,
                           total_requests, total_tokens, updated_at
                    FROM organization_balance
                    WHERE organization_id = $1
                    "#,
                    &[&organization_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row_opt.map(|row| self.row_to_balance(&row)))
    }

    /// Count total usage history records for an organization
    pub async fn count_usage_history(&self, organization_id: Uuid) -> Result<i64> {
        let row = retry_db!("count_organization_usage_history", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    SELECT COUNT(*) as count
                    FROM organization_usage_log
                    WHERE organization_id = $1
                    "#,
                    &[&organization_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.get::<_, i64>("count"))
    }

    /// Get usage history for an organization
    pub async fn get_usage_history(
        &self,
        organization_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<OrganizationUsageLog>> {
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        let rows = retry_db!("get_organization_usage_history", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    SELECT ul.*,
                        ul.total_cost - COALESCE((SELECT SUM(amount)::BIGINT
                            FROM usage_credit_adjustments adjustment
                            WHERE adjustment.inference_usage_id = ul.id), 0)
                            AS filtered_total_cost,
                        CASE WHEN ul.funded_amount IS NULL THEN NULL ELSE
                            COALESCE((SELECT jsonb_agg(jsonb_build_object(
                                'type', a.credit_type, 'amount', a.amount - COALESCE((SELECT SUM(amount)::BIGINT
                                    FROM usage_credit_allocation_reversals reversal
                                    WHERE reversal.allocation_id = a.id), 0), 'source', a.source,
                                'organization_limit_id', a.organization_limit_id,
                                'policy_version', a.policy_version
                            ) ORDER BY a.priority_position)
                            FROM usage_credit_allocations a
                            WHERE a.inference_usage_id = ul.id
                              AND a.amount > COALESCE((SELECT SUM(amount)::BIGINT
                                  FROM usage_credit_allocation_reversals reversal
                                  WHERE reversal.allocation_id = a.id), 0)), '[]'::jsonb)
                        END AS credit_allocations
                    FROM organization_usage_log ul
                    WHERE ul.organization_id = $1
                    ORDER BY ul.created_at DESC
                    LIMIT $2 OFFSET $3
                    "#,
                    &[&organization_id, &limit, &offset],
                )
                .await
                .map_err(map_db_error)
        })?;

        rows.iter()
            .map(|row| self.row_to_usage_log(row, true, None))
            .collect()
    }

    /// Count total usage history records for an API key
    pub async fn count_usage_history_by_api_key(
        &self,
        api_key_id: Uuid,
        credit_type: Option<&str>,
    ) -> Result<i64> {
        let row = retry_db!("count_usage_history_by_api_key", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    SELECT COUNT(*) as count
                    FROM organization_usage_log
                    WHERE api_key_id = $1
                      AND ($2::TEXT IS NULL OR EXISTS (
                          SELECT 1 FROM usage_credit_allocations a
                          WHERE a.inference_usage_id = organization_usage_log.id
                            AND a.credit_type = $2
                            AND a.amount > COALESCE((SELECT SUM(amount)::BIGINT
                                FROM usage_credit_allocation_reversals reversal
                                WHERE reversal.allocation_id = a.id), 0)))
                    "#,
                    &[&api_key_id, &credit_type],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.get::<_, i64>("count"))
    }

    /// Get usage history for a specific API key
    pub async fn get_usage_history_by_api_key(
        &self,
        api_key_id: Uuid,
        credit_type: Option<&str>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<OrganizationUsageLog>> {
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        let rows = retry_db!("get_usage_history_by_api_key", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    SELECT ul.*,
                        CASE WHEN $2::TEXT IS NULL THEN ul.total_cost - COALESCE((
                            SELECT SUM(amount)::BIGINT FROM usage_credit_adjustments adjustment
                            WHERE adjustment.inference_usage_id = ul.id
                        ), 0) ELSE
                            (SELECT a.amount - COALESCE((SELECT SUM(amount)::BIGINT
                                 FROM usage_credit_allocation_reversals reversal
                                 WHERE reversal.allocation_id = a.id), 0)
                             FROM usage_credit_allocations a
                             WHERE a.inference_usage_id = ul.id AND a.credit_type = $2)
                        END AS filtered_total_cost,
                        CASE WHEN ul.funded_amount IS NULL THEN NULL ELSE
                            COALESCE((SELECT jsonb_agg(jsonb_build_object(
                                'type', a.credit_type, 'amount', a.amount - COALESCE((SELECT SUM(amount)::BIGINT
                                    FROM usage_credit_allocation_reversals reversal
                                    WHERE reversal.allocation_id = a.id), 0), 'source', a.source,
                                'organization_limit_id', a.organization_limit_id,
                                'policy_version', a.policy_version
                            ) ORDER BY a.priority_position)
                            FROM usage_credit_allocations a
                            WHERE a.inference_usage_id = ul.id
                              AND ($2::TEXT IS NULL OR a.credit_type = $2)
                              AND a.amount > COALESCE((SELECT SUM(amount)::BIGINT
                                  FROM usage_credit_allocation_reversals reversal
                                  WHERE reversal.allocation_id = a.id), 0)), '[]'::jsonb)
                        END AS credit_allocations
                    FROM organization_usage_log ul
                    WHERE ul.api_key_id = $1
                      AND ($2::TEXT IS NULL OR EXISTS (
                          SELECT 1 FROM usage_credit_allocations filter_allocation
                          WHERE filter_allocation.inference_usage_id = ul.id
                            AND filter_allocation.credit_type = $2
                            AND filter_allocation.amount > COALESCE((SELECT SUM(amount)::BIGINT
                                FROM usage_credit_allocation_reversals reversal
                                WHERE reversal.allocation_id = filter_allocation.id), 0)))
                    ORDER BY ul.created_at DESC
                    LIMIT $3 OFFSET $4
                    "#,
                    &[&api_key_id, &credit_type, &limit, &offset],
                )
                .await
                .map_err(map_db_error)
        })?;

        rows.iter()
            .map(|row| self.row_to_usage_log(row, true, None))
            .collect()
    }

    /// Get usage statistics for a time period
    pub async fn get_usage_stats(
        &self,
        organization_id: Uuid,
        start_date: chrono::DateTime<Utc>,
        end_date: chrono::DateTime<Utc>,
    ) -> Result<UsageStats> {
        let row = retry_db!("get_organization_usage_stats", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    SELECT
                        COUNT(*) as request_count,
                        SUM(total_tokens) as total_tokens,
                        SUM(total_cost) as total_cost
                    FROM organization_usage_log
                    WHERE organization_id = $1
                      AND created_at >= $2
                      AND created_at <= $3
                    "#,
                    &[&organization_id, &start_date, &end_date],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(UsageStats {
            request_count: row.get::<_, i64>(0),
            total_tokens: row.get::<_, Option<i64>>(1).unwrap_or(0),
            total_cost: row.get::<_, Option<i64>>(2).unwrap_or(0),
        })
    }

    /// Aggregate usage by model for an organization since `start_date`.
    pub async fn get_usage_by_model_since(
        &self,
        organization_id: Uuid,
        start_date: chrono::DateTime<Utc>,
    ) -> Result<Vec<UsageByModel>> {
        let rows = retry_db!("get_organization_usage_by_model", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    SELECT
                        model_name,
                        COALESCE(SUM(input_tokens), 0)::BIGINT  AS input_tokens,
                        COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens,
                        COALESCE(SUM(total_tokens), 0)::BIGINT  AS total_tokens,
                        COALESCE(SUM(total_cost), 0)::BIGINT    AS total_cost,
                        COUNT(*)::BIGINT                        AS request_count
                    FROM organization_usage_log
                    WHERE organization_id = $1
                      AND created_at >= $2
                    GROUP BY model_name
                    ORDER BY total_cost DESC
                    "#,
                    &[&organization_id, &start_date],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows
            .into_iter()
            .map(|row| UsageByModel {
                model: row.get("model_name"),
                input_tokens: row.get("input_tokens"),
                output_tokens: row.get("output_tokens"),
                total_tokens: row.get("total_tokens"),
                total_cost: row.get("total_cost"),
                request_count: row.get("request_count"),
            })
            .collect())
    }

    fn row_to_usage_log(
        &self,
        row: &Row,
        was_inserted: bool,
        allocations_override: Option<Vec<services::usage::CreditAllocation>>,
    ) -> Result<OrganizationUsageLog> {
        // Parse stop_reason from string to enum
        let stop_reason_str: Option<String> = row.get("stop_reason");
        let stop_reason = stop_reason_str.as_deref().map(StopReason::parse);

        // Convert response_id from UUID to ResponseId
        let response_id_uuid: Option<Uuid> = row.get("response_id");
        let response_id = response_id_uuid.map(ResponseId::from);
        let served_provider_tier = parse_served_provider_tier(row.get("served_provider_tier"))?;
        let served_provider_type = parse_served_provider_type(row.get("served_provider_type"))?;

        let credit_allocations = match allocations_override {
            Some(value) => Some(value),
            None => row
                .try_get::<_, Option<serde_json::Value>>("credit_allocations")
                .ok()
                .flatten()
                .map(serde_json::from_value)
                .transpose()?,
        };
        let total_cost = row
            .try_get("filtered_total_cost")
            .unwrap_or_else(|_| row.get("total_cost"));
        let (funded_amount, unfunded_amount) = if row
            .try_get::<_, Option<i64>>("funded_amount")
            .ok()
            .flatten()
            .is_some()
        {
            let funded = credit_allocations
                .as_ref()
                .map(|allocations| allocations.iter().map(|allocation| allocation.amount).sum())
                .unwrap_or(0);
            (Some(funded), Some(total_cost - funded))
        } else {
            (None, None)
        };

        Ok(OrganizationUsageLog {
            id: row.get("id"),
            organization_id: row.get("organization_id"),
            workspace_id: row.get("workspace_id"),
            api_key_id: row.get("api_key_id"),
            model_id: row.get("model_id"),
            model: row.get("model_name"),
            input_tokens: row.get("input_tokens"),
            output_tokens: row.get("output_tokens"),
            cache_read_tokens: row.get("cache_read_tokens"),
            cache_write_tokens: row.get("cache_write_tokens"),
            billing_details: row.try_get("billing_details").ok().flatten(),
            service_tier: row.try_get("service_tier").ok().flatten(),
            context_band: row.try_get("context_band").ok().flatten(),
            total_tokens: row.get("total_tokens"),
            input_cost: row.get("input_cost"),
            output_cost: row.get("output_cost"),
            total_cost,
            inference_type: row.get("inference_type"),
            created_at: row.get("created_at"),
            ttft_ms: row.get("ttft_ms"),
            avg_itl_ms: row.get("avg_itl_ms"),
            inference_id: row.get("inference_id"),
            provider_request_id: row.get("provider_request_id"),
            stop_reason,
            response_id,
            image_count: row.get("image_count"),
            served_provider_tier,
            served_provider_type,
            served_via_fallback: row.get("served_via_fallback"),
            was_inserted,
            credit_allocations,
            funded_amount,
            unfunded_amount,
            allocation_policy_version: row.try_get("allocation_policy_version").ok().flatten(),
        })
    }

    fn row_to_balance(&self, row: &Row) -> OrganizationBalance {
        OrganizationBalance {
            organization_id: row.get("organization_id"),
            total_spent: row.get("total_spent"),
            last_usage_at: row.get("last_usage_at"),
            total_requests: row.get("total_requests"),
            total_tokens: row.get("total_tokens"),
            updated_at: row.get("updated_at"),
        }
    }

    /// Get the stop reason for a specific response ID
    /// Used to check if a response was stopped due to client disconnect
    pub async fn get_stop_reason_by_response_id(
        &self,
        response_id: Uuid,
    ) -> Result<Option<StopReason>> {
        let row_opt = retry_db!("get_stop_reason_by_response_id", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"SELECT stop_reason FROM organization_usage_log WHERE response_id = $1"#,
                    &[&response_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row_opt.and_then(|row| {
            let stop_reason_str: Option<String> = row.get("stop_reason");
            stop_reason_str.as_deref().map(StopReason::parse)
        }))
    }

    /// Get the stop reason for a specific provider request ID (e.g., chatcmpl-xxx)
    /// Used to check if a chat completion was stopped due to client disconnect
    pub async fn get_stop_reason_by_provider_request_id(
        &self,
        provider_request_id: &str,
    ) -> Result<Option<StopReason>> {
        let row_opt = retry_db!("get_stop_reason_by_provider_request_id", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"SELECT stop_reason FROM organization_usage_log WHERE provider_request_id = $1"#,
                    &[&provider_request_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row_opt.and_then(|row| {
            let stop_reason_str: Option<String> = row.get("stop_reason");
            stop_reason_str.as_deref().map(StopReason::parse)
        }))
    }

    /// Get costs by inference IDs (for HuggingFace billing integration)
    /// Returns one entry per requested inference_id that was found for the
    /// organization; callers decide how to represent the missing ones.
    pub async fn get_costs_by_inference_ids(
        &self,
        organization_id: Uuid,
        inference_ids: Vec<Uuid>,
    ) -> Result<Vec<services::usage::InferenceCost>> {
        if inference_ids.is_empty() {
            return Ok(vec![]);
        }

        let rows = retry_db!("get_costs_by_inference_ids", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    SELECT inference_id, total_cost
                    FROM organization_usage_log
                    WHERE organization_id = $1 AND inference_id = ANY($2)
                    "#,
                    &[&organization_id, &inference_ids],
                )
                .await
                .map_err(map_db_error)
        })?;

        // Collapse into one entry per inference_id (retried requests can
        // share an id; preserve the pre-existing last-row-wins behavior).
        let found_costs: HashMap<Uuid, i64> = rows
            .iter()
            .filter_map(|row| {
                let inference_id: Option<Uuid> = row.get("inference_id");
                let total_cost: i64 = row.get("total_cost");
                inference_id.map(|id| (id, total_cost))
            })
            .collect();

        Ok(found_costs
            .into_iter()
            .map(
                |(inference_id, cost_nano_usd)| services::usage::InferenceCost {
                    inference_id,
                    cost_nano_usd,
                },
            )
            .collect())
    }
}

#[derive(Debug, Clone)]
pub struct UsageStats {
    pub request_count: i64,
    pub total_tokens: i64,
    pub total_cost: i64,
}

#[derive(Debug, Clone)]
pub struct UsageByModel {
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub total_cost: i64,
    pub request_count: i64,
}

fn parse_served_provider_tier(value: Option<String>) -> Result<Option<ServedProviderTier>> {
    value
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(|message| anyhow::anyhow!("Invalid served_provider_tier in usage log: {message}"))
}

fn parse_served_provider_type(value: Option<String>) -> Result<Option<ServedProviderType>> {
    value
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(|message| anyhow::anyhow!("Invalid served_provider_type in usage log: {message}"))
}
