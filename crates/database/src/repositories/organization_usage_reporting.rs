use crate::repositories::{utils::map_db_error, OrganizationUsageRepository};
use crate::retry_db;
use anyhow::{Context, Result};
use services::common::RepositoryError;
use services::usage::{
    InferenceUsageHistoryQuery, InferenceUsageReportQuery, InferenceUsageReportRow,
};
use tokio_postgres::Row;

impl OrganizationUsageRepository {
    pub async fn list_inference_usage_report(
        &self,
        query: InferenceUsageReportQuery,
    ) -> Result<Vec<InferenceUsageReportRow>> {
        validate_query(&query)?;

        let cursor = query.cursor;
        let cursor_created_at = cursor.map(|value| value.created_at);
        let cursor_id = cursor.map(|value| value.id);
        let limit = i64::from(query.limit);
        let deadline = crate::repositories::reporting_query::reporting_deadline(
            self.reporting_statement_timeout,
            query.deadline,
        )?;

        let rows = retry_db!("list_inference_usage_report", {
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
                        id, organization_id, workspace_id, api_key_id, created_at,
                        model_name, inference_type, input_tokens, output_tokens,
                        cache_read_tokens, cache_write_tokens, total_tokens, input_cost, output_cost,
                        CASE WHEN $8::TEXT IS NULL THEN total_cost - COALESCE((
                            SELECT SUM(amount)::BIGINT FROM usage_credit_adjustments adjustment
                            WHERE adjustment.inference_usage_id = organization_usage_log.id
                        ), 0) ELSE
                            (SELECT a.amount - COALESCE((
                                SELECT SUM(amount)::BIGINT FROM usage_credit_allocation_reversals reversal
                                WHERE reversal.allocation_id = a.id
                             ), 0) FROM usage_credit_allocations a
                             WHERE a.inference_usage_id = organization_usage_log.id
                               AND a.credit_type = $8)
                        END AS total_cost,
                        response_id, provider_request_id, inference_id,
                        stop_reason, image_count, service_tier, context_band, billing_details,
                        funded_amount, unfunded_amount, allocation_policy_version,
                        CASE WHEN funded_amount IS NULL THEN NULL ELSE
                            COALESCE((SELECT jsonb_agg(jsonb_build_object(
                                'type', a.credit_type, 'amount', a.amount - COALESCE((SELECT SUM(amount)::BIGINT
                                    FROM usage_credit_allocation_reversals reversal
                                    WHERE reversal.allocation_id = a.id), 0), 'source', a.source,
                                'organization_limit_id', a.organization_limit_id,
                                'policy_version', a.policy_version
                            ) ORDER BY a.priority_position)
                            FROM usage_credit_allocations a
                            WHERE a.inference_usage_id = organization_usage_log.id
                              AND ($8::TEXT IS NULL OR a.credit_type = $8)
                              AND a.amount > COALESCE((SELECT SUM(amount)::BIGINT
                                  FROM usage_credit_allocation_reversals reversal
                                  WHERE reversal.allocation_id = a.id), 0)), '[]'::jsonb)
                        END AS credit_allocations
                    FROM organization_usage_log
                    WHERE organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
                      AND ($4::UUID IS NULL OR workspace_id = $4)
                      AND ($5::UUID IS NULL OR api_key_id = $5)
                      AND ($6::TEXT IS NULL OR model_name = $6)
                      AND ($7::TEXT IS NULL OR inference_type = $7)
                      AND ($8::TEXT IS NULL OR EXISTS (
                          SELECT 1 FROM usage_credit_allocations allocation_filter
                          WHERE allocation_filter.inference_usage_id = organization_usage_log.id
                            AND allocation_filter.credit_type = $8
                            AND allocation_filter.amount > COALESCE((
                                SELECT SUM(amount)::BIGINT FROM usage_credit_allocation_reversals reversal
                                WHERE reversal.allocation_id = allocation_filter.id
                            ), 0)
                      ))
                      AND (
                          $9::TIMESTAMPTZ IS NULL
                          OR created_at < $9
                          OR (created_at = $9 AND id < $10::UUID)
                      )
                    ORDER BY created_at DESC, id DESC
                    LIMIT $11
                    "#,
                    &[
                        &query.organization_id,
                        &query.start_time,
                        &query.end_time,
                        &query.workspace_id,
                        &query.api_key_id,
                        &query.model,
                        &query.inference_type,
                        &query.credit_type,
                        &cursor_created_at,
                        &cursor_id,
                        &limit,
                    ],
                )
                .await
                .map_err(map_db_error)?;
            transaction.commit().await.map_err(map_db_error)?;
            Ok::<_, RepositoryError>(rows)
        })?;

        Ok(rows.iter().map(row_to_report).collect())
    }

    pub async fn list_inference_usage_history(
        &self,
        query: InferenceUsageHistoryQuery,
    ) -> Result<(Vec<InferenceUsageReportRow>, i64)> {
        validate_history_query(&query)?;

        let (rows, total) = retry_db!("list_inference_usage_history", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let rows = client
                .query(
                    r#"
                    SELECT
                        id, organization_id, workspace_id, api_key_id, created_at,
                        model_name, inference_type, input_tokens, output_tokens,
                        cache_read_tokens, cache_write_tokens, total_tokens, input_cost, output_cost,
                        CASE WHEN $6::TEXT IS NULL THEN total_cost - COALESCE((
                            SELECT SUM(amount)::BIGINT FROM usage_credit_adjustments adjustment
                            WHERE adjustment.inference_usage_id = organization_usage_log.id
                        ), 0) ELSE
                            (SELECT a.amount - COALESCE((
                                SELECT SUM(amount)::BIGINT FROM usage_credit_allocation_reversals reversal
                                WHERE reversal.allocation_id = a.id
                             ), 0) FROM usage_credit_allocations a
                             WHERE a.inference_usage_id = organization_usage_log.id
                               AND a.credit_type = $6)
                        END AS total_cost,
                        response_id, provider_request_id, inference_id,
                        stop_reason, image_count, service_tier, context_band, billing_details,
                        funded_amount, unfunded_amount, allocation_policy_version,
                        CASE WHEN funded_amount IS NULL THEN NULL ELSE
                            COALESCE((SELECT jsonb_agg(jsonb_build_object(
                                'type', a.credit_type, 'amount', a.amount - COALESCE((SELECT SUM(amount)::BIGINT
                                    FROM usage_credit_allocation_reversals reversal
                                    WHERE reversal.allocation_id = a.id), 0), 'source', a.source,
                                'organization_limit_id', a.organization_limit_id,
                                'policy_version', a.policy_version
                            ) ORDER BY a.priority_position)
                            FROM usage_credit_allocations a
                            WHERE a.inference_usage_id = organization_usage_log.id
                              AND ($6::TEXT IS NULL OR a.credit_type = $6)
                              AND a.amount > COALESCE((SELECT SUM(amount)::BIGINT
                                  FROM usage_credit_allocation_reversals reversal
                                  WHERE reversal.allocation_id = a.id), 0)), '[]'::jsonb)
                        END AS credit_allocations
                    FROM organization_usage_log
                    WHERE organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
                      AND ($4::UUID IS NULL OR workspace_id = $4)
                      AND ($5::UUID IS NULL OR api_key_id = $5)
                      AND ($6::TEXT IS NULL OR EXISTS (
                          SELECT 1 FROM usage_credit_allocations allocation_filter
                          WHERE allocation_filter.inference_usage_id = organization_usage_log.id
                            AND allocation_filter.credit_type = $6
                            AND allocation_filter.amount > COALESCE((
                                SELECT SUM(amount)::BIGINT FROM usage_credit_allocation_reversals reversal
                                WHERE reversal.allocation_id = allocation_filter.id
                            ), 0)
                      ))
                    ORDER BY created_at DESC, id DESC
                    LIMIT $7 OFFSET $8
                    "#,
                    &[
                        &query.organization_id,
                        &query.start_time,
                        &query.end_time,
                        &query.workspace_id,
                        &query.api_key_id,
                        &query.credit_type,
                        &query.limit,
                        &query.offset,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            let count = client
                .query_one(
                    r#"
                    SELECT COUNT(*)::BIGINT AS count
                    FROM organization_usage_log
                    WHERE organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
                      AND ($4::UUID IS NULL OR workspace_id = $4)
                      AND ($5::UUID IS NULL OR api_key_id = $5)
                      AND ($6::TEXT IS NULL OR EXISTS (
                          SELECT 1 FROM usage_credit_allocations allocation_filter
                          WHERE allocation_filter.inference_usage_id = organization_usage_log.id
                            AND allocation_filter.credit_type = $6
                            AND allocation_filter.amount > COALESCE((
                                SELECT SUM(amount)::BIGINT FROM usage_credit_allocation_reversals reversal
                                WHERE reversal.allocation_id = allocation_filter.id
                            ), 0)
                      ))
                    "#,
                    &[
                        &query.organization_id,
                        &query.start_time,
                        &query.end_time,
                        &query.workspace_id,
                        &query.api_key_id,
                        &query.credit_type,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            Ok::<(Vec<Row>, i64), RepositoryError>((rows, count.get("count")))
        })?;

        Ok((rows.iter().map(row_to_report).collect(), total))
    }
}

fn validate_query(query: &InferenceUsageReportQuery) -> Result<()> {
    if query.limit == 0 {
        return Err(RepositoryError::ValidationFailed("limit must be positive".to_string()).into());
    }
    if let (Some(start), Some(end)) = (query.start_time, query.end_time) {
        if end < start {
            return Err(RepositoryError::ValidationFailed(
                "end_time must be greater than or equal to start_time".to_string(),
            )
            .into());
        }
    }
    Ok(())
}

fn validate_history_query(query: &InferenceUsageHistoryQuery) -> Result<()> {
    if query.limit <= 0 {
        return Err(RepositoryError::ValidationFailed("limit must be positive".to_string()).into());
    }
    if query.offset < 0 {
        return Err(
            RepositoryError::ValidationFailed("offset must be non-negative".to_string()).into(),
        );
    }
    if let (Some(start), Some(end)) = (query.start_time, query.end_time) {
        if end < start {
            return Err(RepositoryError::ValidationFailed(
                "end_time must be greater than or equal to start_time".to_string(),
            )
            .into());
        }
    }
    Ok(())
}

fn row_to_report(row: &Row) -> InferenceUsageReportRow {
    let total_cost_nano_usd = row.get("total_cost");
    let credit_allocations: Option<Vec<services::usage::CreditAllocation>> = row
        .try_get::<_, Option<serde_json::Value>>("credit_allocations")
        .ok()
        .flatten()
        .and_then(|value| serde_json::from_value(value).ok());
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
        (Some(funded), Some(total_cost_nano_usd - funded))
    } else {
        (None, None)
    };
    InferenceUsageReportRow {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        workspace_id: row.get("workspace_id"),
        api_key_id: row.get("api_key_id"),
        created_at: row.get("created_at"),
        model: row.get("model_name"),
        inference_type: row.get("inference_type"),
        input_tokens: i64::from(row.get::<_, i32>("input_tokens")),
        output_tokens: i64::from(row.get::<_, i32>("output_tokens")),
        cache_read_tokens: i64::from(row.get::<_, i32>("cache_read_tokens")),
        cache_write_tokens: i64::from(row.get::<_, i32>("cache_write_tokens")),
        service_tier: row.try_get("service_tier").ok().flatten(),
        context_band: row.try_get("context_band").ok().flatten(),
        billing_details: row.try_get("billing_details").ok().flatten(),
        total_tokens: i64::from(row.get::<_, i32>("total_tokens")),
        input_cost_nano_usd: row.get("input_cost"),
        output_cost_nano_usd: row.get("output_cost"),
        cache_read_cost_nano_usd: None,
        total_cost_nano_usd,
        response_id: row.get("response_id"),
        provider_request_id: row.get("provider_request_id"),
        inference_id: row.get("inference_id"),
        stop_reason: row.get("stop_reason"),
        image_count: row.get("image_count"),
        credit_allocations,
        funded_amount,
        unfunded_amount,
        allocation_policy_version: row.try_get("allocation_policy_version").ok().flatten(),
    }
}
