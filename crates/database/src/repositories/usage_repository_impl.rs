use crate::models::RecordUsageRequest;
use crate::repositories::OrganizationUsageRepository;
use services::usage::ports::{InferenceCost, OrganizationBalanceInfo, UsageLogEntry};
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
            model_id: request.model_id,
            model_name: request.model_name,
            input_tokens: request.input_tokens,
            output_tokens: request.output_tokens,
            input_cost: request.input_cost,
            output_cost: request.output_cost,
            total_cost: request.total_cost,
            inference_type: request.inference_type,
            ttft_ms: request.ttft_ms,
            avg_itl_ms: request.avg_itl_ms,
            inference_id: request.inference_id,
        };

        let log = self.record_usage(db_request).await?;

        Ok(UsageLogEntry {
            id: log.id,
            organization_id: log.organization_id,
            workspace_id: log.workspace_id,
            api_key_id: log.api_key_id,
            model_id: log.model_id,
            model: log.model,
            input_tokens: log.input_tokens,
            output_tokens: log.output_tokens,
            total_tokens: log.total_tokens,
            input_cost: log.input_cost,
            output_cost: log.output_cost,
            total_cost: log.total_cost,
            inference_type: log.inference_type,
            created_at: log.created_at,
            ttft_ms: log.ttft_ms,
            avg_itl_ms: log.avg_itl_ms,
            inference_id: log.inference_id,
        })
    }

    async fn get_balance(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationBalanceInfo>> {
        let balance = self.get_balance(organization_id).await?;

        Ok(balance.map(|b| OrganizationBalanceInfo {
            organization_id: b.organization_id,
            total_spent: b.total_spent,
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
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        let logs = self
            .get_usage_history(organization_id, limit, offset)
            .await?;

        let total = self.count_usage_history(organization_id).await?;

        let entries = logs
            .into_iter()
            .map(|log| UsageLogEntry {
                id: log.id,
                organization_id: log.organization_id,
                workspace_id: log.workspace_id,
                api_key_id: log.api_key_id,
                model_id: log.model_id,
                model: log.model,
                input_tokens: log.input_tokens,
                output_tokens: log.output_tokens,
                total_tokens: log.total_tokens,
                input_cost: log.input_cost,
                output_cost: log.output_cost,
                total_cost: log.total_cost,
                inference_type: log.inference_type,
                created_at: log.created_at,
                ttft_ms: log.ttft_ms,
                avg_itl_ms: log.avg_itl_ms,
                inference_id: log.inference_id,
            })
            .collect();

        Ok((entries, total))
    }

    async fn get_usage_history_by_api_key(
        &self,
        api_key_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        let logs = self
            .get_usage_history_by_api_key(api_key_id, limit, offset)
            .await?;

        let total = self.count_usage_history_by_api_key(api_key_id).await?;

        let entries = logs
            .into_iter()
            .map(|log| UsageLogEntry {
                id: log.id,
                organization_id: log.organization_id,
                workspace_id: log.workspace_id,
                api_key_id: log.api_key_id,
                model_id: log.model_id,
                model: log.model,
                input_tokens: log.input_tokens,
                output_tokens: log.output_tokens,
                total_tokens: log.total_tokens,
                input_cost: log.input_cost,
                output_cost: log.output_cost,
                total_cost: log.total_cost,
                inference_type: log.inference_type,
                created_at: log.created_at,
                ttft_ms: log.ttft_ms,
                avg_itl_ms: log.avg_itl_ms,
                inference_id: log.inference_id,
            })
            .collect();

        Ok((entries, total))
    }

    async fn get_api_key_spend(&self, api_key_id: Uuid) -> anyhow::Result<i64> {
        self.get_api_key_spend(api_key_id).await
    }

    async fn get_costs_by_inference_ids(
        &self,
        organization_id: Uuid,
        inference_ids: Vec<Uuid>,
    ) -> anyhow::Result<Vec<InferenceCost>> {
        self.get_costs_by_inference_ids(organization_id, inference_ids)
            .await
    }
}
