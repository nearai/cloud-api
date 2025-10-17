pub mod ports;

pub use ports::*;
use std::sync::Arc;
use uuid::Uuid;

pub struct UsageServiceImpl {
    usage_repository: Arc<dyn UsageRepository>,
    model_repository: Arc<dyn ModelRepository>,
    limits_repository: Arc<dyn OrganizationLimitsRepository>,
    workspace_service: Arc<dyn crate::workspace::WorkspaceServiceTrait>,
}

impl UsageServiceImpl {
    pub fn new(
        usage_repository: Arc<dyn UsageRepository>,
        model_repository: Arc<dyn ModelRepository>,
        limits_repository: Arc<dyn OrganizationLimitsRepository>,
        workspace_service: Arc<dyn crate::workspace::WorkspaceServiceTrait>,
    ) -> Self {
        Self {
            usage_repository,
            model_repository,
            limits_repository,
            workspace_service,
        }
    }
}

#[async_trait::async_trait]
impl UsageServiceTrait for UsageServiceImpl {
    /// Calculate cost for a given model and token usage
    /// All costs use fixed scale of 9 (nano-dollars) and USD currency
    async fn calculate_cost(
        &self,
        model_id: &str,
        input_tokens: i32,
        output_tokens: i32,
    ) -> Result<CostBreakdown, UsageError> {
        // Get model pricing
        let model = self
            .model_repository
            .get_model_by_name(model_id)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to get model: {e}")))?
            .ok_or_else(|| UsageError::ModelNotFound(format!("Model '{model_id}' not found")))?;

        // Calculate costs: tokens * cost_per_token (all in nano-dollars, scale 9)
        let input_cost = (input_tokens as i64) * model.input_cost_per_token;
        let output_cost = (output_tokens as i64) * model.output_cost_per_token;
        let total_cost = input_cost + output_cost;

        Ok(CostBreakdown {
            input_cost,
            output_cost,
            total_cost,
        })
    }

    /// Record usage after an API call completes
    async fn record_usage(&self, request: RecordUsageServiceRequest) -> Result<(), UsageError> {
        // Calculate costs
        let cost = self
            .calculate_cost(
                &request.model_id,
                request.input_tokens,
                request.output_tokens,
            )
            .await?;

        // Create database request
        let db_request = RecordUsageDbRequest {
            organization_id: request.organization_id,
            workspace_id: request.workspace_id,
            api_key_id: request.api_key_id,
            response_id: request.response_id,
            model_id: request.model_id,
            input_tokens: request.input_tokens,
            output_tokens: request.output_tokens,
            input_cost: cost.input_cost,
            output_cost: cost.output_cost,
            total_cost: cost.total_cost,
            request_type: request.request_type,
        };

        // Record in database
        let _log = self
            .usage_repository
            .record_usage(db_request)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to record usage: {e}")))?;

        Ok(())
    }

    /// Check if organization can make an API call (pre-flight check)
    ///
    /// Organizations must have credits (positive balance) to make API calls.
    /// All organizations start with $0, and requests are denied until credits are added.
    async fn check_can_use(&self, organization_id: Uuid) -> Result<UsageCheckResult, UsageError> {
        // Get current balance
        let balance = self
            .usage_repository
            .get_balance(organization_id)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to get balance: {e}")))?;

        // Get current limits
        let limit = self
            .limits_repository
            .get_current_limits(organization_id)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to get limits: {e}")))?;

        match (balance, limit) {
            (Some(balance), Some(limit)) => {
                // Compare amounts - deny if spent >= limit (all in same scale 9)
                if balance.total_spent >= limit.spend_limit {
                    Ok(UsageCheckResult::LimitExceeded {
                        spent: balance.total_spent,
                        limit: limit.spend_limit,
                    })
                } else {
                    Ok(UsageCheckResult::Allowed {
                        remaining: limit.spend_limit - balance.total_spent,
                    })
                }
            }
            (Some(_balance), None) => {
                // Has spent money but no limit set - DENY
                // Organizations must have limits set to use the API
                Ok(UsageCheckResult::NoLimitSet)
            }
            (None, Some(limit)) => {
                // No usage yet, but limit exists
                // Check if limit is > 0 (has credits)
                if limit.spend_limit > 0 {
                    Ok(UsageCheckResult::Allowed {
                        remaining: limit.spend_limit,
                    })
                } else {
                    // Limit is set to 0 - no credits
                    Ok(UsageCheckResult::NoCredits)
                }
            }
            (None, None) => {
                // No balance and no limit - DENY (no credits)
                // Organizations must purchase credits before using the API
                Ok(UsageCheckResult::NoCredits)
            }
        }
    }

    /// Get current balance for an organization
    async fn get_balance(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<OrganizationBalanceInfo>, UsageError> {
        let balance = self
            .usage_repository
            .get_balance(organization_id)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to get balance: {e}")))?;

        Ok(balance.map(|b| OrganizationBalanceInfo {
            organization_id: b.organization_id,
            total_spent: b.total_spent,
            last_usage_at: b.last_usage_at,
            total_requests: b.total_requests,
            total_tokens: b.total_tokens,
            updated_at: b.updated_at,
        }))
    }

    /// Get usage history for an organization
    async fn get_usage_history(
        &self,
        organization_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
        let (logs, total) = self
            .usage_repository
            .get_usage_history(organization_id, limit, offset)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to get usage history: {e}")))?;

        Ok((logs, total))
    }

    /// Get current spending limit for an organization
    async fn get_limit(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<OrganizationLimit>, UsageError> {
        let limit = self
            .limits_repository
            .get_current_limits(organization_id)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to get limits: {e}")))?;

        Ok(limit)
    }

    /// Get usage history for a specific API key
    async fn get_usage_history_by_api_key(
        &self,
        api_key_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
        let (logs, total) = self
            .usage_repository
            .get_usage_history_by_api_key(api_key_id, limit, offset)
            .await
            .map_err(|e| {
                UsageError::InternalError(format!("Failed to get API key usage history: {e}"))
            })?;

        Ok((logs, total))
    }

    /// Get usage history for a specific API key with permission checking
    async fn get_api_key_usage_history_with_permissions(
        &self,
        workspace_id: Uuid,
        api_key_id: Uuid,
        user_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
        // Check if the user has permission to access this workspace's API keys
        let can_access = self
            .workspace_service
            .can_manage_api_keys(
                crate::workspace::WorkspaceId(workspace_id),
                crate::auth::UserId(user_id),
            )
            .await
            .map_err(|e| {
                UsageError::InternalError(format!("Failed to check workspace permissions: {e}"))
            })?;

        if !can_access {
            return Err(UsageError::Unauthorized(
                "Access denied to this workspace".to_string(),
            ));
        }

        // Get the API key through the workspace service to verify it exists and belongs to the workspace
        let _api_key = self
            .workspace_service
            .get_api_key(
                crate::workspace::WorkspaceId(workspace_id),
                crate::workspace::ApiKeyId(api_key_id.to_string()),
                crate::auth::UserId(user_id),
            )
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to get API key: {e}")))?
            .ok_or_else(|| {
                UsageError::NotFound("API key not found in this workspace".to_string())
            })?;

        // Get the usage history
        let (logs, total) = self
            .usage_repository
            .get_usage_history_by_api_key(api_key_id, limit, offset)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to get usage history: {e}")))?;

        Ok((logs, total))
    }
}
