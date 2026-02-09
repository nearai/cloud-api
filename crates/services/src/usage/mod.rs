pub mod ports;

use crate::metrics::{
    consts::{get_environment, METRIC_COST_USD, TAG_ENVIRONMENT, TAG_MODEL},
    MetricsServiceTrait,
};
pub use ports::*;
use std::sync::Arc;
use uuid::Uuid;

pub struct UsageServiceImpl {
    usage_repository: Arc<dyn UsageRepository>,
    model_repository: Arc<dyn ModelRepository>,
    limits_repository: Arc<dyn OrganizationLimitsRepository>,
    workspace_service: Arc<dyn crate::workspace::WorkspaceServiceTrait>,
    metrics_service: Arc<dyn MetricsServiceTrait>,
}

impl UsageServiceImpl {
    pub fn new(
        usage_repository: Arc<dyn UsageRepository>,
        model_repository: Arc<dyn ModelRepository>,
        limits_repository: Arc<dyn OrganizationLimitsRepository>,
        workspace_service: Arc<dyn crate::workspace::WorkspaceServiceTrait>,
        metrics_service: Arc<dyn MetricsServiceTrait>,
    ) -> Self {
        Self {
            usage_repository,
            model_repository,
            limits_repository,
            workspace_service,
            metrics_service,
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
        // Use checked arithmetic to prevent integer overflow in billing-critical path
        let input_cost = (input_tokens as i64)
            .checked_mul(model.input_cost_per_token)
            .ok_or_else(|| {
                UsageError::CostCalculationOverflow(format!(
                    "Input cost calculation overflow: {} tokens * {} cost_per_token",
                    input_tokens, model.input_cost_per_token
                ))
            })?;
        let output_cost = (output_tokens as i64)
            .checked_mul(model.output_cost_per_token)
            .ok_or_else(|| {
                UsageError::CostCalculationOverflow(format!(
                    "Output cost calculation overflow: {} tokens * {} cost_per_token",
                    output_tokens, model.output_cost_per_token
                ))
            })?;
        let total_cost = input_cost.checked_add(output_cost).ok_or_else(|| {
            UsageError::CostCalculationOverflow(format!(
                "Total cost calculation overflow: {} + {}",
                input_cost, output_cost
            ))
        })?;

        Ok(CostBreakdown {
            input_cost,
            output_cost,
            total_cost,
        })
    }

    /// Record usage after an API call completes
    async fn record_usage(
        &self,
        request: RecordUsageServiceRequest,
    ) -> Result<UsageLogEntry, UsageError> {
        // Look up the model to get pricing (model_id is already a UUID)
        let model = self
            .model_repository
            .get_model_by_id(request.model_id)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to get model: {e}")))?
            .ok_or_else(|| {
                UsageError::ModelNotFound(format!("Model with ID '{}' not found", request.model_id))
            })?;

        // Calculate costs based on inference type
        let (input_cost, output_cost, total_cost) = match request.inference_type {
            ports::InferenceType::ImageGeneration | ports::InferenceType::ImageEdit => {
                // For image-based operations: use image_count and cost_per_image
                let image_count = request.image_count.unwrap_or(0);
                // Use checked arithmetic to prevent integer overflow in billing-critical path
                let image_cost = (image_count as i64)
                    .checked_mul(model.cost_per_image)
                    .ok_or_else(|| {
                        UsageError::CostCalculationOverflow(format!(
                            "Image cost calculation overflow: {} images * {} cost_per_image",
                            image_count, model.cost_per_image
                        ))
                    })?;
                (0, image_cost, image_cost)
            }
            ports::InferenceType::Rerank => {
                // For rerank: use input tokens as the billing unit
                // Rerank models should set their input_cost_per_token appropriately for the billing model
                // (e.g., cost per token, cost per document, cost per query, etc.)
                // Use checked arithmetic to prevent integer overflow in billing-critical path
                let rerank_cost = (request.input_tokens as i64)
                    .checked_mul(model.input_cost_per_token)
                    .ok_or_else(|| {
                        UsageError::CostCalculationOverflow(format!(
                            "Rerank cost calculation overflow: {} tokens * {} cost_per_token",
                            request.input_tokens, model.input_cost_per_token
                        ))
                    })?;
                (rerank_cost, 0, rerank_cost)
            }
            _ => {
                // For token-based models (chat completions, etc.)
                // Use checked arithmetic to prevent integer overflow in billing-critical path
                let input_cost = (request.input_tokens as i64)
                    .checked_mul(model.input_cost_per_token)
                    .ok_or_else(|| {
                        UsageError::CostCalculationOverflow(format!(
                            "Input cost calculation overflow: {} tokens * {} cost_per_token",
                            request.input_tokens, model.input_cost_per_token
                        ))
                    })?;
                let output_cost = (request.output_tokens as i64)
                    .checked_mul(model.output_cost_per_token)
                    .ok_or_else(|| {
                        UsageError::CostCalculationOverflow(format!(
                            "Output cost calculation overflow: {} tokens * {} cost_per_token",
                            request.output_tokens, model.output_cost_per_token
                        ))
                    })?;
                let total_cost = input_cost.checked_add(output_cost).ok_or_else(|| {
                    UsageError::CostCalculationOverflow(format!(
                        "Total cost calculation overflow: {} + {}",
                        input_cost, output_cost
                    ))
                })?;
                (input_cost, output_cost, total_cost)
            }
        };

        // Create database request with model UUID and name (denormalized)
        let db_request = RecordUsageDbRequest {
            organization_id: request.organization_id,
            workspace_id: request.workspace_id,
            api_key_id: request.api_key_id,
            model_id: request.model_id,
            model_name: model.model_name.clone(),
            input_tokens: request.input_tokens,
            output_tokens: request.output_tokens,
            input_cost,
            output_cost,
            total_cost,
            inference_type: request.inference_type,
            ttft_ms: request.ttft_ms,
            avg_itl_ms: request.avg_itl_ms,
            inference_id: request.inference_id,
            provider_request_id: request.provider_request_id,
            stop_reason: request.stop_reason,
            response_id: request.response_id,
            image_count: request.image_count,
        };

        // Record in database
        let log = self
            .usage_repository
            .record_usage(db_request)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to record usage: {e}")))?;

        // Record cost metric (low-cardinality: model + environment only)
        if total_cost > 0 {
            let environment = get_environment();
            let tags = [
                format!("{}:{}", TAG_MODEL, model.model_name),
                format!("{TAG_ENVIRONMENT}:{environment}"),
            ];
            let tags_str: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
            self.metrics_service
                .record_count(METRIC_COST_USD, total_cost, &tags_str);
        }

        Ok(log)
    }

    /// Record usage from the public API endpoint.
    /// Resolves model by name, validates per-variant fields, and delegates to `record_usage`.
    async fn record_usage_from_api(
        &self,
        organization_id: Uuid,
        workspace_id: Uuid,
        api_key_id: Uuid,
        request: RecordUsageApiRequest,
    ) -> Result<UsageLogEntry, UsageError> {
        let (model_name, input_tokens, output_tokens, image_count, inference_type, external_id) =
            match &request {
                RecordUsageApiRequest::ChatCompletion {
                    model,
                    input_tokens,
                    output_tokens,
                    id,
                } => {
                    if id.trim().is_empty() {
                        return Err(UsageError::ValidationError(
                            "id must be a non-empty string".into(),
                        ));
                    }
                    let input = input_tokens.unwrap_or(0);
                    let output = output_tokens.unwrap_or(0);
                    if input < 0 || output < 0 {
                        return Err(UsageError::ValidationError(
                            "token counts must be non-negative".into(),
                        ));
                    }
                    if input == 0 && output == 0 {
                        return Err(UsageError::ValidationError(
                            "at least one of input_tokens or output_tokens must be positive".into(),
                        ));
                    }
                    (
                        model.clone(),
                        input,
                        output,
                        None,
                        InferenceType::ChatCompletion,
                        id.clone(),
                    )
                }
                RecordUsageApiRequest::ImageGeneration {
                    model,
                    image_count,
                    id,
                } => {
                    if id.trim().is_empty() {
                        return Err(UsageError::ValidationError(
                            "id must be a non-empty string".into(),
                        ));
                    }
                    if *image_count <= 0 {
                        return Err(UsageError::ValidationError(
                            "image_count must be positive".into(),
                        ));
                    }
                    (
                        model.clone(),
                        0,
                        0,
                        Some(*image_count),
                        InferenceType::ImageGeneration,
                        id.clone(),
                    )
                }
            };

        // Look up model by name to get pricing and UUID
        let model = self
            .model_repository
            .get_model_by_name(&model_name)
            .await
            .map_err(|e| UsageError::InternalError(format!("Failed to look up model: {e}")))?
            .ok_or_else(|| {
                UsageError::ModelNotFound(format!("Model '{}' not found", model_name))
            })?;

        // Derive inference tracking fields from the required external `id`.
        // Stored as provider_request_id and hashed to a deterministic UUID v5
        // for inference_id (same logic as the inference pipeline).
        // The inference_id serves as the idempotency key: duplicate calls with
        // the same id within the same org return the existing record.
        let provider_request_id = Some(external_id.clone());
        let inference_id = Some(crate::completions::hash_inference_id_to_uuid(&external_id));

        // Build internal request and delegate.
        // Internal metrics (ttft_ms, avg_itl_ms, stop_reason) are not exposed
        // via the public API — they are populated only by the inference pipeline.
        let service_request = RecordUsageServiceRequest {
            organization_id,
            workspace_id,
            api_key_id,
            model_id: model.id,
            input_tokens,
            output_tokens,
            inference_type,
            ttft_ms: None,
            avg_itl_ms: None,
            inference_id,
            provider_request_id,
            stop_reason: None,
            response_id: None,
            image_count,
        };

        self.record_usage(service_request).await
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

    /// Get costs by inference IDs (for HuggingFace billing integration)
    async fn get_costs_by_inference_ids(
        &self,
        organization_id: Uuid,
        inference_ids: Vec<Uuid>,
    ) -> Result<Vec<InferenceCost>, UsageError> {
        let results = self
            .usage_repository
            .get_costs_by_inference_ids(organization_id, inference_ids)
            .await
            .map_err(|e| {
                UsageError::InternalError(format!("Failed to get costs by inference IDs: {e}"))
            })?;

        // Log count of inference IDs that were not found (cost = 0)
        let not_found_count = results.iter().filter(|ic| ic.cost_nano_usd == 0).count();
        if not_found_count > 0 {
            tracing::error!(
                "Inference IDs not found in usage log: count={}",
                not_found_count
            );
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_cost_calculation_overflow_detection() {
        // This test verifies that i64::checked_mul properly detects overflow conditions
        // These values are used to ensure overflow detection works in billing calculations

        // Maximum i64 value: 9,223,372,036,854,775,807
        let max_i64 = i64::MAX;

        // Test case 1: Very large token count * large cost_per_token should overflow
        // Note: 1B tokens * 10B nano-dollars = 10^18 which exceeds i64::MAX (~9.2 * 10^18)
        let huge_tokens: i64 = 1_000_000_000; // 1B tokens
        let huge_cost: i64 = 10_000_000_000; // 10B nano-dollars per token
        let result = huge_tokens.checked_mul(huge_cost);
        assert!(
            result.is_none(),
            "Expected overflow for {} * {}",
            huge_tokens,
            huge_cost
        );

        // Test case 2: Max i64 * 2 should definitely overflow
        let result = max_i64.checked_mul(2);
        assert!(result.is_none(), "Expected overflow for i64::MAX * 2");

        // Test case 3: Normal reasonable values should NOT overflow
        let normal_tokens: i64 = 100_000; // 100k tokens
        let normal_cost: i64 = 50_000; // 50k nano-dollars per token (reasonable for expensive model)
        let result = normal_tokens.checked_mul(normal_cost);
        assert_eq!(
            result,
            Some(5_000_000_000_i64),
            "Normal calculation should work: {} * {} = 5B",
            normal_tokens,
            normal_cost
        );

        // Test case 4: Verify addition overflow detection
        let val1 = i64::MAX;
        let val2 = 1_i64;
        let result = val1.checked_add(val2);
        assert!(result.is_none(), "Expected overflow for i64::MAX + 1");

        // Test case 5: Normal addition should work
        let result = 1000_i64.checked_add(2000);
        assert_eq!(result, Some(3000));
    }

    #[test]
    fn test_image_cost_overflow_detection() {
        // Test image cost calculation with extreme values
        // Each image can cost up to several billion nano-dollars

        let normal_images: i64 = 10; // 10 images
        let normal_cost_per_image: i64 = 100_000_000; // 100M nano-dollars per image
        let result = normal_images.checked_mul(normal_cost_per_image);
        assert_eq!(
            result,
            Some(1_000_000_000_i64),
            "Normal image cost should work: 10 * 100M = 1B"
        );

        // Extremely expensive image cost that overflows
        // i64::MAX ~= 9.2 * 10^18, so 1B images * 10B cost = 10^18 which still fits
        // But 10B images * 1T cost = 10^21 which overflows
        let many_images: i64 = 10_000_000_000; // 10B images (unrealistic but tests overflow)
        let expensive_cost: i64 = 1_000_000_000_000; // 1T nano-dollars per image
        let result = many_images.checked_mul(expensive_cost);
        assert!(
            result.is_none(),
            "Expected overflow for 10B images * 1T cost"
        );
    }

    #[test]
    fn test_rerank_cost_overflow_detection() {
        // Rerank billing uses input tokens as the unit
        // Test with realistic and unrealistic token counts

        let normal_tokens: i64 = 1_000_000; // 1M tokens (capped max)
        let normal_cost: i64 = 100_000; // 100k nano-dollars per token
        let result = normal_tokens.checked_mul(normal_cost);
        assert_eq!(
            result,
            Some(100_000_000_000_i64),
            "1M tokens * 100k should work"
        );

        // Extremely expensive cost that would overflow
        // 1B tokens * 10B cost/token = 10^18 which fits, but use bigger numbers
        let huge_tokens: i64 = 1_000_000_000_000; // 1T tokens (unrealistic)
        let huge_cost: i64 = 10_000_000_000; // 10B nano-dollars per token
        let result = huge_tokens.checked_mul(huge_cost);
        assert!(result.is_none(), "1T tokens * 10B cost should overflow");
    }
}
