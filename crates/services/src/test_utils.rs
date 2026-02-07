// Test utilities for services crate
use crate::{
    attestation::{
        models::{AttestationReport, SignatureLookupResult},
        ports::AttestationServiceTrait,
        AttestationError,
    },
    usage::{
        CostBreakdown, OrganizationBalanceInfo, OrganizationLimit, RecordUsageApiRequest,
        RecordUsageServiceRequest, UsageCheckResult, UsageError, UsageLogEntry, UsageServiceTrait,
    },
};
use async_trait::async_trait;
use uuid::Uuid;

pub struct MockAttestationService;

#[async_trait]
impl AttestationServiceTrait for MockAttestationService {
    async fn get_chat_signature(
        &self,
        _chat_id: &str,
        _signing_algo: Option<String>,
    ) -> Result<SignatureLookupResult, AttestationError> {
        Err(AttestationError::InternalError(
            "Not implemented".to_string(),
        ))
    }

    async fn store_chat_signature_from_provider(
        &self,
        _chat_id: &str,
    ) -> Result<(), AttestationError> {
        Ok(())
    }

    async fn store_response_signature(
        &self,
        _response_id: &str,
        _request_hash: String,
        _response_hash: String,
    ) -> Result<(), AttestationError> {
        Ok(())
    }

    async fn get_attestation_report(
        &self,
        _model: Option<String>,
        _signing_algo: Option<String>,
        _nonce: Option<String>,
        _signing_address: Option<String>,
    ) -> Result<AttestationReport, AttestationError> {
        Err(AttestationError::InternalError(
            "Not implemented".to_string(),
        ))
    }

    async fn verify_vpc_signature(
        &self,
        _timestamp: i64,
        _signature: String,
    ) -> Result<bool, AttestationError> {
        Ok(false)
    }
}

pub struct MockUsageService;

#[async_trait]
impl UsageServiceTrait for MockUsageService {
    async fn calculate_cost(
        &self,
        _model_id: &str,
        _input_tokens: i32,
        _output_tokens: i32,
    ) -> Result<CostBreakdown, UsageError> {
        Ok(CostBreakdown {
            input_cost: 0,
            output_cost: 0,
            total_cost: 0,
        })
    }

    async fn record_usage(
        &self,
        _request: RecordUsageServiceRequest,
    ) -> Result<UsageLogEntry, UsageError> {
        Ok(UsageLogEntry {
            id: Uuid::new_v4(),
            organization_id: _request.organization_id,
            workspace_id: _request.workspace_id,
            api_key_id: _request.api_key_id,
            model_id: _request.model_id,
            model: String::new(),
            input_tokens: _request.input_tokens,
            output_tokens: _request.output_tokens,
            total_tokens: _request.input_tokens + _request.output_tokens,
            input_cost: 0,
            output_cost: 0,
            total_cost: 0,
            inference_type: _request.inference_type,
            created_at: chrono::Utc::now(),
            ttft_ms: _request.ttft_ms,
            avg_itl_ms: _request.avg_itl_ms,
            inference_id: _request.inference_id,
            provider_request_id: _request.provider_request_id,
            stop_reason: _request.stop_reason,
            response_id: _request.response_id,
            image_count: _request.image_count,
        })
    }

    async fn record_usage_from_api(
        &self,
        organization_id: Uuid,
        workspace_id: Uuid,
        api_key_id: Uuid,
        request: RecordUsageApiRequest,
    ) -> Result<UsageLogEntry, UsageError> {
        let (model, input_tokens, output_tokens, image_count, inference_type) = match &request {
            RecordUsageApiRequest::ChatCompletion {
                model,
                input_tokens,
                output_tokens,
                ..
            } => (
                model.clone(),
                input_tokens.unwrap_or(0),
                output_tokens.unwrap_or(0),
                None,
                "chat_completion".to_string(),
            ),
            RecordUsageApiRequest::ImageGeneration {
                model, image_count, ..
            } => (
                model.clone(),
                0,
                0,
                Some(*image_count),
                "image_generation".to_string(),
            ),
        };
        Ok(UsageLogEntry {
            id: Uuid::new_v4(),
            organization_id,
            workspace_id,
            api_key_id,
            model_id: Uuid::nil(),
            model,
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
            input_cost: 0,
            output_cost: 0,
            total_cost: 0,
            inference_type,
            created_at: chrono::Utc::now(),
            ttft_ms: None,
            avg_itl_ms: None,
            inference_id: None,
            provider_request_id: None,
            stop_reason: None,
            response_id: None,
            image_count,
        })
    }

    async fn check_can_use(&self, _organization_id: Uuid) -> Result<UsageCheckResult, UsageError> {
        Ok(UsageCheckResult::Allowed { remaining: 1000 })
    }

    async fn get_balance(
        &self,
        _organization_id: Uuid,
    ) -> Result<Option<OrganizationBalanceInfo>, UsageError> {
        Ok(None)
    }

    async fn get_usage_history(
        &self,
        _organization_id: Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
        Ok((vec![], 0))
    }

    async fn get_limit(
        &self,
        _organization_id: Uuid,
    ) -> Result<Option<OrganizationLimit>, UsageError> {
        Ok(None)
    }

    async fn get_usage_history_by_api_key(
        &self,
        _api_key_id: Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
        Ok((vec![], 0))
    }

    async fn get_api_key_usage_history_with_permissions(
        &self,
        _workspace_id: Uuid,
        _api_key_id: Uuid,
        _user_id: Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
        Ok((vec![], 0))
    }

    async fn get_costs_by_inference_ids(
        &self,
        _organization_id: Uuid,
        _inference_ids: Vec<Uuid>,
    ) -> Result<Vec<crate::usage::InferenceCost>, UsageError> {
        Ok(vec![])
    }
}

/// A usage service that captures requests for testing
pub struct CapturingUsageService {
    requests: std::sync::Mutex<Vec<RecordUsageServiceRequest>>,
}

impl CapturingUsageService {
    pub fn new() -> Self {
        Self {
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn get_requests(&self) -> Vec<RecordUsageServiceRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl UsageServiceTrait for CapturingUsageService {
    async fn calculate_cost(
        &self,
        _model_id: &str,
        _input_tokens: i32,
        _output_tokens: i32,
    ) -> Result<CostBreakdown, UsageError> {
        Ok(CostBreakdown {
            input_cost: 0,
            output_cost: 0,
            total_cost: 0,
        })
    }

    async fn record_usage(
        &self,
        request: RecordUsageServiceRequest,
    ) -> Result<UsageLogEntry, UsageError> {
        let entry = UsageLogEntry {
            id: Uuid::new_v4(),
            organization_id: request.organization_id,
            workspace_id: request.workspace_id,
            api_key_id: request.api_key_id,
            model_id: request.model_id,
            model: String::new(),
            input_tokens: request.input_tokens,
            output_tokens: request.output_tokens,
            total_tokens: request.input_tokens + request.output_tokens,
            input_cost: 0,
            output_cost: 0,
            total_cost: 0,
            inference_type: request.inference_type.clone(),
            created_at: chrono::Utc::now(),
            ttft_ms: request.ttft_ms,
            avg_itl_ms: request.avg_itl_ms,
            inference_id: request.inference_id,
            provider_request_id: request.provider_request_id.clone(),
            stop_reason: request.stop_reason.clone(),
            response_id: request.response_id.clone(),
            image_count: request.image_count,
        };
        self.requests.lock().unwrap().push(request);
        Ok(entry)
    }

    async fn record_usage_from_api(
        &self,
        organization_id: Uuid,
        workspace_id: Uuid,
        api_key_id: Uuid,
        request: RecordUsageApiRequest,
    ) -> Result<UsageLogEntry, UsageError> {
        let (model, input_tokens, output_tokens, image_count, inference_type) = match &request {
            RecordUsageApiRequest::ChatCompletion {
                model,
                input_tokens,
                output_tokens,
                ..
            } => (
                model.clone(),
                input_tokens.unwrap_or(0),
                output_tokens.unwrap_or(0),
                None,
                "chat_completion".to_string(),
            ),
            RecordUsageApiRequest::ImageGeneration {
                model, image_count, ..
            } => (
                model.clone(),
                0,
                0,
                Some(*image_count),
                "image_generation".to_string(),
            ),
        };
        Ok(UsageLogEntry {
            id: Uuid::new_v4(),
            organization_id,
            workspace_id,
            api_key_id,
            model_id: Uuid::nil(),
            model,
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
            input_cost: 0,
            output_cost: 0,
            total_cost: 0,
            inference_type,
            created_at: chrono::Utc::now(),
            ttft_ms: None,
            avg_itl_ms: None,
            inference_id: None,
            provider_request_id: None,
            stop_reason: None,
            response_id: None,
            image_count,
        })
    }

    async fn check_can_use(&self, _organization_id: Uuid) -> Result<UsageCheckResult, UsageError> {
        Ok(UsageCheckResult::Allowed { remaining: 1000 })
    }

    async fn get_balance(
        &self,
        _organization_id: Uuid,
    ) -> Result<Option<OrganizationBalanceInfo>, UsageError> {
        Ok(None)
    }

    async fn get_usage_history(
        &self,
        _organization_id: Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
        Ok((vec![], 0))
    }

    async fn get_limit(
        &self,
        _organization_id: Uuid,
    ) -> Result<Option<OrganizationLimit>, UsageError> {
        Ok(None)
    }

    async fn get_usage_history_by_api_key(
        &self,
        _api_key_id: Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
        Ok((vec![], 0))
    }

    async fn get_api_key_usage_history_with_permissions(
        &self,
        _workspace_id: Uuid,
        _api_key_id: Uuid,
        _user_id: Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
        Ok((vec![], 0))
    }

    async fn get_costs_by_inference_ids(
        &self,
        _organization_id: Uuid,
        _inference_ids: Vec<Uuid>,
    ) -> Result<Vec<crate::usage::InferenceCost>, UsageError> {
        Ok(vec![])
    }
}
