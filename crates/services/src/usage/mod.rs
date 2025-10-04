pub mod ports;

pub use ports::*;
use std::sync::Arc;
use uuid::Uuid;

pub struct UsageServiceImpl {
    usage_repository: Arc<dyn UsageRepository>,
    model_repository: Arc<dyn ModelRepository>,
    limits_repository: Arc<dyn OrganizationLimitsRepository>,
}

impl UsageServiceImpl {
    pub fn new(
        usage_repository: Arc<dyn UsageRepository>,
        model_repository: Arc<dyn ModelRepository>,
        limits_repository: Arc<dyn OrganizationLimitsRepository>,
    ) -> Self {
        Self {
            usage_repository,
            model_repository,
            limits_repository,
        }
    }
}

#[async_trait::async_trait]
impl UsageService for UsageServiceImpl {
    /// Calculate cost for a given model and token usage
    async fn calculate_cost(
        &self,
        model_id: &str,
        input_tokens: u32,
        output_tokens: u32,
    ) -> Result<CostBreakdown, UsageError> {
        // Get model pricing
        let model = self
            .model_repository
            .get_model_by_name(model_id)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to get model: {}", e)))?
            .ok_or_else(|| UsageError::ModelNotFound(format!("Model '{}' not found", model_id)))?;

        // Calculate costs: (tokens * cost_per_token_amount)
        // The cost is already in the smallest unit (e.g., micro-dollars if scale=6)
        let input_cost = (input_tokens as i64) * model.input_cost_amount;
        let output_cost = (output_tokens as i64) * model.output_cost_amount;
        let total_cost = input_cost + output_cost;

        Ok(CostBreakdown {
            input_cost_amount: input_cost,
            input_cost_scale: model.input_cost_scale,
            input_cost_currency: model.input_cost_currency.clone(),
            output_cost_amount: output_cost,
            output_cost_scale: model.output_cost_scale,
            output_cost_currency: model.output_cost_currency.clone(),
            total_cost_amount: total_cost,
            total_cost_scale: model.input_cost_scale, // Use same scale for total
            total_cost_currency: model.input_cost_currency.clone(),
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
            input_tokens: request.input_tokens as i32,
            output_tokens: request.output_tokens as i32,
            input_cost_amount: cost.input_cost_amount,
            input_cost_scale: cost.input_cost_scale,
            input_cost_currency: cost.input_cost_currency,
            output_cost_amount: cost.output_cost_amount,
            output_cost_scale: cost.output_cost_scale,
            output_cost_currency: cost.output_cost_currency,
            total_cost_amount: cost.total_cost_amount,
            total_cost_scale: cost.total_cost_scale,
            total_cost_currency: cost.total_cost_currency,
            request_type: request.request_type,
        };

        // Record in database
        let _log = self
            .usage_repository
            .record_usage(db_request)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to record usage: {}", e)))?;

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
            .map_err(|e| UsageError::InternalError(format!("Failed to get balance: {}", e)))?;

        // Get current limits
        let limit = self
            .limits_repository
            .get_current_limits(organization_id)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to get limits: {}", e)))?;

        match (balance, limit) {
            (Some(balance), Some(limit)) => {
                // Check if currencies match
                if balance.total_spent_currency != limit.spend_limit_currency {
                    return Ok(UsageCheckResult::CurrencyMismatch {
                        spent_currency: balance.total_spent_currency,
                        limit_currency: limit.spend_limit_currency,
                    });
                }

                // Check if scales match
                if balance.total_spent_scale != limit.spend_limit_scale {
                    // Convert to same scale (this is simplified, production would need proper decimal math)
                    return Err(UsageError::InternalError(
                        "Scale mismatch between balance and limit".to_string(),
                    ));
                }

                // Compare amounts - deny if spent >= limit
                if balance.total_spent_amount >= limit.spend_limit_amount {
                    Ok(UsageCheckResult::LimitExceeded {
                        spent_amount: balance.total_spent_amount,
                        spent_scale: balance.total_spent_scale,
                        spent_currency: balance.total_spent_currency,
                        limit_amount: limit.spend_limit_amount,
                        limit_scale: limit.spend_limit_scale,
                        limit_currency: limit.spend_limit_currency,
                    })
                } else {
                    Ok(UsageCheckResult::Allowed {
                        remaining_amount: limit.spend_limit_amount - balance.total_spent_amount,
                        remaining_scale: balance.total_spent_scale,
                        remaining_currency: balance.total_spent_currency,
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
                if limit.spend_limit_amount > 0 {
                    Ok(UsageCheckResult::Allowed {
                        remaining_amount: limit.spend_limit_amount,
                        remaining_scale: limit.spend_limit_scale,
                        remaining_currency: limit.spend_limit_currency,
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
            .map_err(|e| UsageError::InternalError(format!("Failed to get balance: {}", e)))?;

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

    /// Get usage history for an organization
    async fn get_usage_history(
        &self,
        organization_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<UsageLogEntry>, UsageError> {
        let logs = self
            .usage_repository
            .get_usage_history(organization_id, limit, offset)
            .await
            .map_err(|e| {
                UsageError::InternalError(format!("Failed to get usage history: {}", e))
            })?;

        Ok(logs)
    }
}
