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
                        cache_read_tokens, total_tokens, input_cost, output_cost,
                        total_cost, response_id, provider_request_id, inference_id,
                        stop_reason, image_count
                    FROM organization_usage_log
                    WHERE organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
                      AND ($4::UUID IS NULL OR workspace_id = $4)
                      AND ($5::UUID IS NULL OR api_key_id = $5)
                      AND ($6::TEXT IS NULL OR model_name = $6)
                      AND ($7::TEXT IS NULL OR inference_type = $7)
                      AND (
                          $8::TIMESTAMPTZ IS NULL
                          OR created_at < $8
                          OR (created_at = $8 AND id < $9::UUID)
                      )
                    ORDER BY created_at DESC, id DESC
                    LIMIT $10
                    "#,
                    &[
                        &query.organization_id,
                        &query.start_time,
                        &query.end_time,
                        &query.workspace_id,
                        &query.api_key_id,
                        &query.model,
                        &query.inference_type,
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
                        cache_read_tokens, total_tokens, input_cost, output_cost,
                        total_cost, response_id, provider_request_id, inference_id,
                        stop_reason, image_count
                    FROM organization_usage_log
                    WHERE organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
                      AND ($4::UUID IS NULL OR workspace_id = $4)
                      AND ($5::UUID IS NULL OR api_key_id = $5)
                    ORDER BY created_at DESC, id DESC
                    LIMIT $6 OFFSET $7
                    "#,
                    &[
                        &query.organization_id,
                        &query.start_time,
                        &query.end_time,
                        &query.workspace_id,
                        &query.api_key_id,
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
                    "#,
                    &[
                        &query.organization_id,
                        &query.start_time,
                        &query.end_time,
                        &query.workspace_id,
                        &query.api_key_id,
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
        total_tokens: i64::from(row.get::<_, i32>("total_tokens")),
        input_cost_nano_usd: row.get("input_cost"),
        output_cost_nano_usd: row.get("output_cost"),
        cache_read_cost_nano_usd: None,
        total_cost_nano_usd: row.get("total_cost"),
        response_id: row.get("response_id"),
        provider_request_id: row.get("provider_request_id"),
        inference_id: row.get("inference_id"),
        stop_reason: row.get("stop_reason"),
        image_count: row.get("image_count"),
    }
}
