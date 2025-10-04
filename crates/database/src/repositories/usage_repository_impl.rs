use crate::models::RecordUsageRequest;
use crate::repositories::OrganizationUsageRepository;
use services::usage::ports::{OrganizationBalanceInfo, UsageLogEntry};
use uuid::Uuid;

/// Trait implementation adapter for UsageRepository
#[async_trait::async_trait]
impl services::usage::ports::UsageRepository for OrganizationUsageRepository {
    async fn record_usage(
        &self,
        request: services::usage::ports::RecordUsageDbRequest,
    ) -> anyhow::Result<UsageLogEntry> {
        let db_request = RecordUsageRequest {
            organization_id: request.organization_id,
            workspace_id: request.workspace_id,
            api_key_id: request.api_key_id,
            response_id: request.response_id,
            model_id: request.model_id,
            input_tokens: request.input_tokens,
            output_tokens: request.output_tokens,
            input_cost_amount: request.input_cost_amount,
            input_cost_scale: request.input_cost_scale,
            input_cost_currency: request.input_cost_currency,
            output_cost_amount: request.output_cost_amount,
            output_cost_scale: request.output_cost_scale,
            output_cost_currency: request.output_cost_currency,
            total_cost_amount: request.total_cost_amount,
            total_cost_scale: request.total_cost_scale,
            total_cost_currency: request.total_cost_currency,
            request_type: request.request_type,
        };

        let log = self.record_usage(db_request).await?;

        Ok(UsageLogEntry {
            id: log.id,
            organization_id: log.organization_id,
            workspace_id: log.workspace_id,
            api_key_id: log.api_key_id,
            response_id: log.response_id,
            model_id: log.model_id,
            input_tokens: log.input_tokens,
            output_tokens: log.output_tokens,
            total_tokens: log.total_tokens,
            input_cost_amount: log.input_cost_amount,
            input_cost_scale: log.input_cost_scale,
            input_cost_currency: log.input_cost_currency,
            output_cost_amount: log.output_cost_amount,
            output_cost_scale: log.output_cost_scale,
            output_cost_currency: log.output_cost_currency,
            total_cost_amount: log.total_cost_amount,
            total_cost_scale: log.total_cost_scale,
            total_cost_currency: log.total_cost_currency,
            request_type: log.request_type,
            created_at: log.created_at,
        })
    }

    async fn get_balance(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationBalanceInfo>> {
        let balance = self.get_balance(organization_id).await?;

        Ok(balance.map(|b| OrganizationBalanceInfo {
            organization_id: b.organization_id,
            total_spent_amount: b.total_spent_amount,
            total_spent_scale: b.total_spent_scale,
            total_spent_currency: b.total_spent_currency,
            last_usage_at: b.last_usage_at,
            total_requests: b.total_requests,
            total_tokens: b.total_tokens,
            updated_at: b.updated_at,
        }))
    }

    async fn get_usage_history(
        &self,
        organization_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> anyhow::Result<Vec<UsageLogEntry>> {
        let logs = self
            .get_usage_history(organization_id, limit, offset)
            .await?;

        Ok(logs
            .into_iter()
            .map(|log| UsageLogEntry {
                id: log.id,
                organization_id: log.organization_id,
                workspace_id: log.workspace_id,
                api_key_id: log.api_key_id,
                response_id: log.response_id,
                model_id: log.model_id,
                input_tokens: log.input_tokens,
                output_tokens: log.output_tokens,
                total_tokens: log.total_tokens,
                input_cost_amount: log.input_cost_amount,
                input_cost_scale: log.input_cost_scale,
                input_cost_currency: log.input_cost_currency,
                output_cost_amount: log.output_cost_amount,
                output_cost_scale: log.output_cost_scale,
                output_cost_currency: log.output_cost_currency,
                total_cost_amount: log.total_cost_amount,
                total_cost_scale: log.total_cost_scale,
                total_cost_currency: log.total_cost_currency,
                request_type: log.request_type,
                created_at: log.created_at,
            })
            .collect())
    }
}

