// Test utilities for services crate
#![cfg(test)]

use crate::{
    attestation::{
        models::{AttestationReport, ChatSignature},
        ports::AttestationServiceTrait,
        AttestationError,
    },
    usage::{
        CostBreakdown, OrganizationBalanceInfo, OrganizationLimit, RecordUsageServiceRequest,
        UsageCheckResult, UsageError, UsageLogEntry, UsageServiceTrait,
    },
};
use async_trait::async_trait;
use uuid::Uuid;

pub struct MockAttestationService;

#[async_trait]
impl AttestationServiceTrait for MockAttestationService {
    async fn get_chat_signature(&self, _chat_id: &str) -> Result<ChatSignature, AttestationError> {
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
        _signing_algo: Option<String>,
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

    async fn record_usage(&self, _request: RecordUsageServiceRequest) -> Result<(), UsageError> {
        Ok(())
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

    async fn record_usage(&self, request: RecordUsageServiceRequest) -> Result<(), UsageError> {
        self.requests.lock().unwrap().push(request);
        Ok(())
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
}
