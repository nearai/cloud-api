//! Characterization of how `UsageServiceImpl::record_usage` prices each
//! inference type and what it hands to the repository.

use super::*;
use crate::metrics::capturing::CapturingMetricsService;
use crate::metrics::consts::METRIC_BILLING_PRICING_FALLBACK;
use crate::workspace::MockWorkspaceServiceTrait;
use std::sync::Mutex;

/// Captures the persisted request and echoes it back as an inserted row.
#[derive(Default)]
struct CapturingUsageRepository {
    requests: Mutex<Vec<RecordUsageDbRequest>>,
}

#[async_trait::async_trait]
impl UsageRepository for CapturingUsageRepository {
    async fn record_usage(&self, request: RecordUsageDbRequest) -> anyhow::Result<UsageLogEntry> {
        let entry = UsageLogEntry {
            id: Uuid::new_v4(),
            organization_id: request.organization_id,
            workspace_id: request.workspace_id,
            api_key_id: request.api_key_id,
            model_id: request.model_id,
            model: request.model_name.clone(),
            input_tokens: request.input_tokens,
            output_tokens: request.output_tokens,
            cache_read_tokens: request.cache_read_tokens,
            cache_write_tokens: request.cache_write_tokens,
            billing_details: request.billing_details.clone(),
            service_tier: request.service_tier.clone(),
            context_band: request.context_band.clone(),
            total_tokens: request.input_tokens + request.output_tokens,
            input_cost: request.input_cost,
            output_cost: request.output_cost,
            total_cost: request.total_cost,
            inference_type: request.inference_type,
            created_at: chrono::Utc::now(),
            ttft_ms: request.ttft_ms,
            avg_itl_ms: request.avg_itl_ms,
            inference_id: request.inference_id,
            provider_request_id: request.provider_request_id.clone(),
            stop_reason: request.stop_reason.clone(),
            response_id: request.response_id.clone(),
            image_count: request.image_count,
            was_inserted: true,
            credit_allocations: None,
            funded_amount: None,
            unfunded_amount: None,
            allocation_policy_version: None,
            provider_attribution: request.provider_attribution,
        };
        self.requests.lock().unwrap().push(request);
        Ok(entry)
    }

    async fn get_balance(&self, _: Uuid) -> anyhow::Result<Option<OrganizationBalanceInfo>> {
        unimplemented!()
    }

    async fn get_usage_history(
        &self,
        _: Uuid,
        _: Option<i64>,
        _: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        unimplemented!()
    }

    async fn get_usage_history_by_api_key(
        &self,
        _: Uuid,
        _: Option<&str>,
        _: Option<i64>,
        _: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        unimplemented!()
    }

    async fn get_api_key_spend(&self, _: Uuid) -> anyhow::Result<i64> {
        unimplemented!()
    }

    async fn get_costs_by_inference_ids(
        &self,
        _: Uuid,
        _: Vec<Uuid>,
    ) -> anyhow::Result<Vec<InferenceCost>> {
        unimplemented!()
    }

    async fn get_stop_reason_by_response_id(&self, _: Uuid) -> anyhow::Result<Option<StopReason>> {
        unimplemented!()
    }

    async fn get_stop_reason_by_provider_request_id(
        &self,
        _: &str,
    ) -> anyhow::Result<Option<StopReason>> {
        unimplemented!()
    }

    async fn get_usage_by_model(
        &self,
        _: Uuid,
        _: chrono::DateTime<chrono::Utc>,
        _: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<Vec<UsageByModelEntry>> {
        unimplemented!()
    }

    async fn list_inference_usage_report(
        &self,
        _: InferenceUsageReportQuery,
    ) -> anyhow::Result<Vec<InferenceUsageReportRow>> {
        unimplemented!()
    }

    async fn list_inference_usage_history(
        &self,
        _: InferenceUsageHistoryQuery,
    ) -> anyhow::Result<(Vec<InferenceUsageReportRow>, i64)> {
        unimplemented!()
    }
}

struct FixedModelRepository(ModelPricing);

#[async_trait::async_trait]
impl ModelRepository for FixedModelRepository {
    async fn get_model_by_name(&self, _: &str) -> anyhow::Result<Option<ModelPricing>> {
        unimplemented!()
    }

    async fn get_model_by_id(&self, _: Uuid) -> anyhow::Result<Option<ModelPricing>> {
        Ok(Some(self.0.clone()))
    }
}

struct UnusedLimitsRepository;

#[async_trait::async_trait]
impl OrganizationLimitsRepository for UnusedLimitsRepository {
    async fn get_current_limits(&self, _: Uuid) -> anyhow::Result<Option<OrganizationLimit>> {
        unimplemented!()
    }

    async fn get_current_limit_breakdown(
        &self,
        _: Uuid,
    ) -> anyhow::Result<Vec<OrganizationCreditLimit>> {
        unimplemented!()
    }
}

fn model() -> ModelPricing {
    ModelPricing {
        id: Uuid::new_v4(),
        model_name: "pricing-model".to_string(),
        input_cost_per_token: 3,
        output_cost_per_token: 7,
        cost_per_image: 11,
        cache_read_cost_per_token: Some(1),
        text_pricing: None,
    }
}

/// One profile tier with no cache-write price, so a positive profiled
/// cache-write count trips the critical pricing fallback.
fn profile_without_cache_write() -> TextPricingProfile {
    TextPricingProfile::from_json(serde_json::json!({
        "version": 1,
        "currency": "USD",
        "unit": "million_tokens",
        "tiers": {"default": {"short": {
            "uncachedInput": "5.00",
            "cachedInput": "0.50",
            "output": "30.00"
        }}}
    }))
    .unwrap()
}

/// Every field set to a non-default value so a dropped or swapped field in
/// pricing shows up.
fn request(inference_type: InferenceType) -> RecordUsageServiceRequest {
    RecordUsageServiceRequest {
        organization_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
        api_key_id: Uuid::new_v4(),
        model_id: Uuid::new_v4(),
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 30,
        cache_write: Some(CacheWriteBilling {
            tokens: 10,
            cost_per_token: 5,
        }),
        profiled_cache_write_tokens: 15,
        requested_service_tier: None,
        provider_service_tier: None,
        inference_type,
        ttft_ms: Some(12),
        avg_itl_ms: Some(1.5),
        inference_id: Some(Uuid::new_v4()),
        provider_request_id: Some("provider-id".to_string()),
        stop_reason: Some(StopReason::Completed),
        response_id: None,
        image_count: Some(4),
        provider_attribution: ProviderAttribution::default(),
        discount: None,
    }
}

struct Recorded {
    db: RecordUsageDbRequest,
    metrics: Vec<crate::metrics::capturing::RecordedMetric>,
}

async fn record(model: ModelPricing, request: RecordUsageServiceRequest) -> Recorded {
    let repo = Arc::new(CapturingUsageRepository::default());
    let metrics = Arc::new(CapturingMetricsService::new());
    let service = UsageServiceImpl::new(
        repo.clone(),
        Arc::new(FixedModelRepository(model)),
        Arc::new(UnusedLimitsRepository),
        Arc::new(MockWorkspaceServiceTrait::new()),
        metrics.clone(),
        Vec::new(),
    );
    service.record_usage(request).await.expect("record_usage");
    let db = repo
        .requests
        .lock()
        .unwrap()
        .pop()
        .expect("one persisted row");
    Recorded {
        db,
        metrics: metrics.get_metrics(),
    }
}

fn costs(db: &RecordUsageDbRequest) -> (i64, i64, i64) {
    (db.input_cost, db.output_cost, db.total_cost)
}

fn assert_unprofiled(db: &RecordUsageDbRequest) {
    assert!(db.billing_details.is_none());
    assert!(db.service_tier.is_none());
    assert!(db.context_band.is_none());
}

#[tokio::test]
async fn images_bill_image_count_times_cost_per_image_as_output() {
    for inference_type in [InferenceType::ImageGeneration, InferenceType::ImageEdit] {
        let recorded = record(model(), request(inference_type)).await;
        assert_eq!(costs(&recorded.db), (0, 44, 44), "{inference_type}");
        // The legacy cache-write subset is persisted even though unpriced.
        assert_eq!(recorded.db.cache_write_tokens, 10);
        assert_eq!(recorded.db.cache_read_tokens, 30);
        assert_unprofiled(&recorded.db);
    }
}

#[tokio::test]
async fn image_without_count_bills_zero() {
    let mut request = request(InferenceType::ImageGeneration);
    request.image_count = None;
    let recorded = record(model(), request).await;
    assert_eq!(costs(&recorded.db), (0, 0, 0));
}

#[tokio::test]
async fn image_cost_overflow_is_an_error() {
    let mut model = model();
    model.cost_per_image = i64::MAX;
    let repo = Arc::new(CapturingUsageRepository::default());
    let service = UsageServiceImpl::new(
        repo.clone(),
        Arc::new(FixedModelRepository(model)),
        Arc::new(UnusedLimitsRepository),
        Arc::new(MockWorkspaceServiceTrait::new()),
        Arc::new(CapturingMetricsService::new()),
        Vec::new(),
    );
    let result = service
        .record_usage(request(InferenceType::ImageGeneration))
        .await;
    assert!(matches!(
        result,
        Err(UsageError::CostCalculationOverflow(_))
    ));
    assert!(repo.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn audio_bills_duration_at_input_rate_without_cache_pricing() {
    let recorded = record(model(), request(InferenceType::AudioTranscription)).await;
    assert_eq!(costs(&recorded.db), (300, 0, 300));
    assert_eq!(recorded.db.cache_write_tokens, 10);
    assert_unprofiled(&recorded.db);
}

#[tokio::test]
async fn input_only_types_bill_input_tokens_without_cache_pricing() {
    for inference_type in [
        InferenceType::Rerank,
        InferenceType::Embedding,
        InferenceType::Score,
        InferenceType::PrivacyClassify,
    ] {
        let recorded = record(model(), request(inference_type)).await;
        assert_eq!(costs(&recorded.db), (300, 0, 300), "{inference_type}");
        assert_eq!(recorded.db.cache_write_tokens, 10);
        assert_unprofiled(&recorded.db);
    }
}

#[tokio::test]
async fn input_only_overflow_names_the_inference_type() {
    let mut model = model();
    model.input_cost_per_token = i64::MAX;
    let repo = Arc::new(CapturingUsageRepository::default());
    let service = UsageServiceImpl::new(
        repo,
        Arc::new(FixedModelRepository(model)),
        Arc::new(UnusedLimitsRepository),
        Arc::new(MockWorkspaceServiceTrait::new()),
        Arc::new(CapturingMetricsService::new()),
        Vec::new(),
    );
    let error = service
        .record_usage(request(InferenceType::Rerank))
        .await
        .expect_err("overflow");
    match error {
        UsageError::CostCalculationOverflow(message) => {
            assert!(
                message.starts_with("rerank cost calculation overflow"),
                "{message}"
            )
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn token_types_without_profile_use_cache_aware_token_cost() {
    for inference_type in [
        InferenceType::ChatCompletion,
        InferenceType::ChatCompletionStream,
        InferenceType::Decisions,
    ] {
        let recorded = record(model(), request(inference_type)).await;
        // (100 - 30 read - 10 write) * 3 + 30 * 1 + 10 * 5 = 260; 20 * 7 = 140
        assert_eq!(costs(&recorded.db), (260, 140, 400), "{inference_type}");
        assert_eq!(recorded.db.cache_write_tokens, 10);
        assert_unprofiled(&recorded.db);
    }
}

#[tokio::test]
async fn cache_counts_are_clamped_to_input_before_pricing() {
    let mut request = request(InferenceType::ChatCompletion);
    request.input_tokens = 25;
    request.cache_read_tokens = 30;
    let recorded = record(model(), request).await;
    assert_eq!(recorded.db.cache_read_tokens, 25);
    assert_eq!(recorded.db.cache_write_tokens, 0);
    // 25 cached * 1 + 20 * 7
    assert_eq!(costs(&recorded.db), (25, 140, 165));
}

#[tokio::test]
async fn profiled_text_uses_profile_snapshot_and_profiled_cache_writes() {
    let mut model = model();
    model.text_pricing = Some(profile_without_cache_write());
    let mut request = request(InferenceType::ChatCompletionStream);
    request.profiled_cache_write_tokens = 0;
    let recorded = record(model, request).await;

    // 70 uncached * $5/M + 30 cached * $0.5/M + 20 * $30/M = $0.000965
    assert_eq!(recorded.db.total_cost, 965_000);
    assert_eq!(
        recorded.db.input_cost + recorded.db.output_cost,
        recorded.db.total_cost
    );
    assert_eq!(recorded.db.cache_write_tokens, 0);
    assert_eq!(recorded.db.service_tier.as_deref(), Some("default"));
    assert_eq!(recorded.db.context_band.as_deref(), Some("short"));
    let details = recorded.db.billing_details.expect("billing snapshot");
    assert_eq!(details["profileVersion"], 1);
    assert!(details.get("criticalFallbackReason").is_none());
    assert!(!recorded
        .metrics
        .iter()
        .any(|m| m.name == METRIC_BILLING_PRICING_FALLBACK));
}

#[tokio::test]
async fn profiled_text_unpriced_cache_write_emits_fallback_metric() {
    let mut model = model();
    model.text_pricing = Some(profile_without_cache_write());
    let recorded = record(model, request(InferenceType::ChatCompletion)).await;

    assert_eq!(recorded.db.cache_write_tokens, 15);
    let details = recorded.db.billing_details.expect("billing snapshot");
    assert_eq!(
        details["criticalFallbackReason"],
        "unpriced_positive_cache_write"
    );
    let fallback: Vec<_> = recorded
        .metrics
        .iter()
        .filter(|m| m.name == METRIC_BILLING_PRICING_FALLBACK)
        .collect();
    assert_eq!(fallback.len(), 1);
    assert!(fallback[0]
        .tags
        .contains(&"model:pricing-model".to_string()));
    assert!(fallback[0]
        .tags
        .contains(&"reason:unpriced_positive_cache_write".to_string()));
}

#[tokio::test]
async fn discount_applies_after_pricing_and_keeps_list_amounts() {
    let mut request = request(InferenceType::Embedding);
    request.discount = Some(UsageDiscount::from_basis_points(5_000).unwrap());
    let recorded = record(model(), request).await;
    assert_eq!(costs(&recorded.db), (150, 0, 150));
    assert!(recorded.db.billing_details.is_some());
}

#[tokio::test]
async fn passthrough_fields_reach_the_repository_unchanged() {
    let request = request(InferenceType::ChatCompletion);
    let expected = request.clone();
    let recorded = record(model(), request).await;
    let db = recorded.db;
    assert_eq!(db.organization_id, expected.organization_id);
    assert_eq!(db.workspace_id, expected.workspace_id);
    assert_eq!(db.api_key_id, expected.api_key_id);
    assert_eq!(db.model_id, expected.model_id);
    assert_eq!(db.model_name, "pricing-model");
    assert_eq!(db.input_tokens, 100);
    assert_eq!(db.output_tokens, 20);
    assert_eq!(db.ttft_ms, Some(12));
    assert_eq!(db.avg_itl_ms, Some(1.5));
    assert_eq!(db.inference_id, expected.inference_id);
    assert_eq!(db.provider_request_id.as_deref(), Some("provider-id"));
    assert_eq!(db.stop_reason, Some(StopReason::Completed));
    assert_eq!(db.image_count, Some(4));
    assert_eq!(db.provider_attribution, expected.provider_attribution);
}
