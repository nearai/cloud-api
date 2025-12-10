// Test utilities for services crate
use crate::{
    attestation::{
        models::{AttestationReport, ChatSignature},
        ports::AttestationServiceTrait,
        AttestationError,
    },
    models::ports::{ModelWithPricing, ModelsError},
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
    async fn get_chat_signature(
        &self,
        _chat_id: &str,
        _signing_algo: Option<String>,
    ) -> Result<ChatSignature, AttestationError> {
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

impl Default for CapturingUsageService {
    fn default() -> Self {
        Self::new()
    }
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

/// Create a mock models service with standard test models
/// Returns a mockall-generated mock pre-configured with models matching the mock inference provider pool
pub fn create_mock_models_service() -> crate::models::ports::MockModelsServiceTrait {
    let mut mock = crate::models::ports::MockModelsServiceTrait::new();

    // Standard test models matching mock inference provider pool
    let standard_models = vec![
        create_test_model("Qwen/Qwen3-30B-A3B-Instruct-2507"),
        create_test_model("Qwen/Qwen2.5-72B-Instruct"),
        create_test_model("zai-org/GLM-4.6"),
        create_test_model("nearai/gpt-oss-120b"),
        create_test_model("dphn/Dolphin-Mistral-24B-Venice-Edition"),
        create_test_model("meta-llama/Llama-3.1-70B-Instruct"),
        create_test_model("meta-llama/Llama-3.1-8B-Instruct"),
    ];

    // Clone for use in closures
    let models_for_pricing = standard_models.clone();
    let models_for_resolve = standard_models.clone();

    // Set up get_models_with_pricing to return standard models
    mock.expect_get_models_with_pricing()
        .returning(move |_limit, _offset| {
            let models = models_for_pricing.clone();
            let total = models.len() as i64;
            Ok((models, total))
        });

    // Set up resolve_and_get_model to find by name
    mock.expect_resolve_and_get_model()
        .returning(move |identifier| {
            models_for_resolve
                .iter()
                .find(|m| m.model_name == identifier)
                .cloned()
                .ok_or(ModelsError::NotFound(identifier.to_string()))
        });

    mock
}

fn create_test_model(name: &str) -> ModelWithPricing {
    ModelWithPricing {
        id: Uuid::new_v4(),
        model_name: name.to_string(),
        model_display_name: format!("{name} (Test)"),
        model_description: "Test model".to_string(),
        input_cost_per_token: 1000000,  // $0.0001 per token
        output_cost_per_token: 2000000, // $0.0002 per token
        context_length: 128000,
        verifiable: true,
        aliases: vec![],
        model_icon: None,
    }
}
