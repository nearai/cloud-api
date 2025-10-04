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
                    input_cost_amount, input_cost_scale, input_cost_currency,
                    output_cost_amount, output_cost_scale, output_cost_currency,
                    total_cost_amount, total_cost_scale, total_cost_currency,
                    request_type, created_at
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)
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
                    &request.input_cost_amount,
                    &request.input_cost_scale,
                    &request.input_cost_currency,
                    &request.output_cost_amount,
                    &request.output_cost_scale,
                    &request.output_cost_currency,
                    &request.total_cost_amount,
                    &request.total_cost_scale,
                    &request.total_cost_currency,
                    &request.request_type,
                    &now,
                ],
            )
            .await
            .context("Failed to insert usage log")?;

        // Normalize cost to balance table scale (scale 6)
        // Balance table uses scale 6, while model costs may use different scales (e.g., scale 9)
        const BALANCE_SCALE: i32 = 6;
        let normalized_cost = if request.total_cost_scale == BALANCE_SCALE {
            request.total_cost_amount
        } else if request.total_cost_scale > BALANCE_SCALE {
            // Scale down: divide by 10^(difference)
            let scale_diff = request.total_cost_scale - BALANCE_SCALE;
            let divisor = 10_i64.pow(scale_diff as u32);
            request.total_cost_amount / divisor
        } else {
            // Scale up: multiply by 10^(difference)
            let scale_diff = BALANCE_SCALE - request.total_cost_scale;
            let multiplier = 10_i64.pow(scale_diff as u32);
            request.total_cost_amount * multiplier
        };

        // Update organization balance
        transaction
            .execute(
                r#"
                INSERT INTO organization_balance (
                    organization_id,
                    total_spent_amount,
                    total_spent_scale,
                    total_spent_currency,
                    last_usage_at,
                    total_requests,
                    total_tokens,
                    updated_at
                ) VALUES ($1, $2, $3, $4, $5, 1, $6, $7)
                ON CONFLICT (organization_id) DO UPDATE SET
                    total_spent_amount = organization_balance.total_spent_amount + $2,
                    total_requests = organization_balance.total_requests + 1,
                    total_tokens = organization_balance.total_tokens + $6,
                    last_usage_at = $5,
                    updated_at = $7
                "#,
                &[
                    &request.organization_id,
                    &normalized_cost,
                    &BALANCE_SCALE,
                    &request.total_cost_currency,
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
                SELECT organization_id, total_spent_amount, total_spent_scale,
                       total_spent_currency, last_usage_at, total_requests,
                       total_tokens, updated_at
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
                       input_cost_amount, input_cost_scale, input_cost_currency,
                       output_cost_amount, output_cost_scale, output_cost_currency,
                       total_cost_amount, total_cost_scale, total_cost_currency,
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
                    SUM(total_cost_amount) as total_cost
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
            input_cost_amount: row.get("input_cost_amount"),
            input_cost_scale: row.get("input_cost_scale"),
            input_cost_currency: row.get("input_cost_currency"),
            output_cost_amount: row.get("output_cost_amount"),
            output_cost_scale: row.get("output_cost_scale"),
            output_cost_currency: row.get("output_cost_currency"),
            total_cost_amount: row.get("total_cost_amount"),
            total_cost_scale: row.get("total_cost_scale"),
            total_cost_currency: row.get("total_cost_currency"),
            request_type: row.get("request_type"),
            created_at: row.get("created_at"),
        }
    }

    fn row_to_balance(&self, row: &Row) -> OrganizationBalance {
        OrganizationBalance {
            organization_id: row.get("organization_id"),
            total_spent_amount: row.get("total_spent_amount"),
            total_spent_scale: row.get("total_spent_scale"),
            total_spent_currency: row.get("total_spent_currency"),
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
