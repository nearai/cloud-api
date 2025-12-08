use crate::models::{OrganizationBalance, OrganizationUsageLog, RecordUsageRequest};
use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use chrono::Utc;
use services::common::RepositoryError;
use tokio_postgres::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct OrganizationUsageRepository {
    pool: DbPool,
}

impl OrganizationUsageRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
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
                    SELECT COALESCE(SUM(total_cost), 0)::BIGINT as total_spend
                    FROM organization_usage_log
                    WHERE api_key_id = $1
                    "#,
                    &[&api_key_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        let total_spend: i64 = row.get("total_spend");
        Ok(total_spend)
    }

    /// Record usage and update balance atomically
    pub async fn record_usage(&self, request: RecordUsageRequest) -> Result<OrganizationUsageLog> {
        let row = retry_db!("record_organization_usage", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let transaction = client.transaction().await.map_err(map_db_error)?;

            let id = Uuid::new_v4();
            let now = Utc::now();
            let total_tokens = request.input_tokens + request.output_tokens;

            // Insert usage log entry (model_name is denormalized for performance)
            let row = transaction
                .query_one(
                    r#"
                    INSERT INTO organization_usage_log (
                        id, organization_id, workspace_id, api_key_id,
                        model_id, model_name, input_tokens, output_tokens, total_tokens,
                        input_cost, output_cost, total_cost,
                        request_type, inference_type, created_at, ttft_ms, avg_itl_ms, inference_id
                    ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
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
                        &total_tokens,
                        &request.input_cost,
                        &request.output_cost,
                        &request.total_cost,
                        &request.inference_type,
                        &request.inference_type,
                        &now,
                        &request.ttft_ms,
                        &request.avg_itl_ms,
                        &request.inference_id,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            // Update organization balance (all costs use fixed scale 9)
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

            Ok::<tokio_postgres::Row, RepositoryError>(row)
        })?;

        Ok(self.row_to_usage_log(&row))
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
                    SELECT
                        id, organization_id, workspace_id, api_key_id,
                        model_id, model_name, input_tokens, output_tokens, total_tokens,
                        input_cost, output_cost, total_cost,
                        inference_type, created_at, ttft_ms, avg_itl_ms, inference_id
                    FROM organization_usage_log
                    WHERE organization_id = $1
                    ORDER BY created_at DESC
                    LIMIT $2 OFFSET $3
                    "#,
                    &[&organization_id, &limit, &offset],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows.iter().map(|row| self.row_to_usage_log(row)).collect())
    }

    /// Count total usage history records for an API key
    pub async fn count_usage_history_by_api_key(&self, api_key_id: Uuid) -> Result<i64> {
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
                    "#,
                    &[&api_key_id],
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
                    SELECT
                        id, organization_id, workspace_id, api_key_id,
                        model_id, model_name, input_tokens, output_tokens, total_tokens,
                        input_cost, output_cost, total_cost,
                        inference_type, created_at, ttft_ms, avg_itl_ms, inference_id
                    FROM organization_usage_log
                    WHERE api_key_id = $1
                    ORDER BY created_at DESC
                    LIMIT $2 OFFSET $3
                    "#,
                    &[&api_key_id, &limit, &offset],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows.iter().map(|row| self.row_to_usage_log(row)).collect())
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

    fn row_to_usage_log(&self, row: &Row) -> OrganizationUsageLog {
        OrganizationUsageLog {
            id: row.get("id"),
            organization_id: row.get("organization_id"),
            workspace_id: row.get("workspace_id"),
            api_key_id: row.get("api_key_id"),
            model_id: row.get("model_id"),
            model: row.get("model_name"),
            input_tokens: row.get("input_tokens"),
            output_tokens: row.get("output_tokens"),
            total_tokens: row.get("total_tokens"),
            input_cost: row.get("input_cost"),
            output_cost: row.get("output_cost"),
            total_cost: row.get("total_cost"),
            inference_type: row.get("inference_type"),
            created_at: row.get("created_at"),
            ttft_ms: row.get("ttft_ms"),
            avg_itl_ms: row.get("avg_itl_ms"),
            inference_id: row.get("inference_id"),
        }
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
}

#[derive(Debug, Clone)]
pub struct UsageStats {
    pub request_count: i64,
    pub total_tokens: i64,
    pub total_cost: i64,
}
