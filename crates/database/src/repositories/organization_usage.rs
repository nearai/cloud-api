use crate::models::{OrganizationBalance, OrganizationUsageLog, RecordUsageRequest};
use crate::pool::DbPool;
use anyhow::{Context, Result};
use chrono::Utc;
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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_one(
                r#"
                SELECT COALESCE(SUM(total_cost), 0)::BIGINT as total_spend
                FROM organization_usage_log
                WHERE api_key_id = $1
                "#,
                &[&api_key_id],
            )
            .await
            .context("Failed to get API key spend")?;

        let total_spend: i64 = row.get("total_spend");
        Ok(total_spend)
    }

    /// Record usage and update balance atomically
    pub async fn record_usage(&self, request: RecordUsageRequest) -> Result<OrganizationUsageLog> {
        let mut client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let transaction = client
            .transaction()
            .await
            .context("Failed to start transaction")?;

        let id = Uuid::new_v4();
        let now = Utc::now();
        let total_tokens = request.input_tokens + request.output_tokens;

        // Insert usage log entry
        let row = transaction
            .query_one(
                r#"
                INSERT INTO organization_usage_log (
                    id, organization_id, workspace_id, api_key_id, response_id,
                    model_id, input_tokens, output_tokens, total_tokens,
                    input_cost, output_cost, total_cost,
                    request_type, created_at
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                RETURNING *
                "#,
                &[
                    &id,
                    &request.organization_id,
                    &request.workspace_id,
                    &request.api_key_id,
                    &request.response_id,
                    &request.model_id,
                    &request.input_tokens,
                    &request.output_tokens,
                    &total_tokens,
                    &request.input_cost,
                    &request.output_cost,
                    &request.total_cost,
                    &request.request_type,
                    &now,
                ],
            )
            .await
            .context("Failed to insert usage log")?;

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
            .context("Failed to update organization balance")?;

        transaction
            .commit()
            .await
            .context("Failed to commit transaction")?;

        Ok(self.row_to_usage_log(&row))
    }

    /// Get current balance for an organization
    pub async fn get_balance(&self, organization_id: Uuid) -> Result<Option<OrganizationBalance>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row_opt = client
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
            .context("Failed to query organization balance")?;

        Ok(row_opt.map(|row| self.row_to_balance(&row)))
    }

    /// Get usage history for an organization
    pub async fn get_usage_history(
        &self,
        organization_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<OrganizationUsageLog>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        let rows = client
            .query(
                r#"
                SELECT id, organization_id, workspace_id, api_key_id, response_id,
                       model_id, input_tokens, output_tokens, total_tokens,
                       input_cost, output_cost, total_cost,
                       request_type, created_at
                FROM organization_usage_log
                WHERE organization_id = $1
                ORDER BY created_at DESC
                LIMIT $2 OFFSET $3
                "#,
                &[&organization_id, &limit, &offset],
            )
            .await
            .context("Failed to query usage history")?;

        Ok(rows.iter().map(|row| self.row_to_usage_log(row)).collect())
    }

    /// Get usage statistics for a time period
    pub async fn get_usage_stats(
        &self,
        organization_id: Uuid,
        start_date: chrono::DateTime<Utc>,
        end_date: chrono::DateTime<Utc>,
    ) -> Result<UsageStats> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
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
            .context("Failed to query usage statistics")?;

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
            response_id: row.get("response_id"),
            model_id: row.get("model_id"),
            input_tokens: row.get("input_tokens"),
            output_tokens: row.get("output_tokens"),
            total_tokens: row.get("total_tokens"),
            input_cost: row.get("input_cost"),
            output_cost: row.get("output_cost"),
            total_cost: row.get("total_cost"),
            request_type: row.get("request_type"),
            created_at: row.get("created_at"),
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
