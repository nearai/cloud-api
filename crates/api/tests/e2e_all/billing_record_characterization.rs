//! Field-level characterization of the `RecordUsageServiceRequest` each
//! non-chat endpoint hands to the usage service.
//!
//! The per-endpoint differences pinned here are deliberate and must survive
//! any refactor of how these requests are built:
//! - `inference_id` is the v5 hash of the provider id for images, score and
//!   System One and privacy classify/redact with a valid upstream id. It is a
//!   random v4 for audio, rerank, embeddings, auto-redact and privacy responses
//!   without a valid upstream id (as returned by the default MockProvider).
//! - A failed record fails the request closed (500) for audio, rerank,
//!   and embeddings; image generation/edit, score, System One and privacy
//!   classify/redact return 200 and retry the identical request asynchronously;
//!   the auto-redact classify charge is best-effort and never retried.

use crate::common::*;
use services::completions::hash_inference_id_to_uuid;
use services::usage::{
    InferenceType, RecordUsageServiceRequest, StopReason, UsageError, UsageServiceTrait,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

/// Wraps the real usage service, captures every `record_usage` request, and
/// can fail the next N recordings before delegating.
struct RecordingUsageService {
    inner: Arc<dyn UsageServiceTrait + Send + Sync>,
    records: Mutex<Vec<RecordUsageServiceRequest>>,
    fail_next: AtomicUsize,
}

impl RecordingUsageService {
    fn records(&self) -> Vec<RecordUsageServiceRequest> {
        self.records
            .lock()
            .expect("records mutex should not poison")
            .clone()
    }

    /// Wait (bounded) until exactly `count` records were captured, then hold
    /// briefly to catch any unexpected extra recording.
    async fn wait_for(&self, count: usize) -> Vec<RecordUsageServiceRequest> {
        // Async retries are spawned immediately on failure (no backoff), so
        // the poll budget only covers task-scheduling jitter; the quiet period
        // absorbs the same jitter before asserting no extra record arrived.
        const POLL_BUDGET: Duration = Duration::from_secs(5);
        const QUIET_PERIOD: Duration = Duration::from_millis(500);
        let deadline = tokio::time::Instant::now() + POLL_BUDGET;
        while self.records().len() < count && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(QUIET_PERIOD).await;
        let records = self.records();
        assert_eq!(records.len(), count, "unexpected record count: {records:?}");
        records
    }
}

#[async_trait::async_trait]
impl UsageServiceTrait for RecordingUsageService {
    async fn record_usage(
        &self,
        request: RecordUsageServiceRequest,
    ) -> Result<services::usage::UsageLogEntry, UsageError> {
        self.records
            .lock()
            .expect("records mutex should not poison")
            .push(request.clone());
        let fail = self
            .fail_next
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if fail {
            return Err(UsageError::InternalError("injected failure".to_string()));
        }
        self.inner.record_usage(request).await
    }

    async fn record_usage_from_api(
        &self,
        organization_id: Uuid,
        workspace_id: Uuid,
        api_key_id: Uuid,
        request: services::usage::RecordUsageApiRequest,
        discount: Option<services::usage::UsageDiscount>,
    ) -> Result<services::usage::UsageLogEntry, UsageError> {
        self.inner
            .record_usage_from_api(organization_id, workspace_id, api_key_id, request, discount)
            .await
    }

    async fn check_can_use(
        &self,
        organization_id: Uuid,
    ) -> Result<services::usage::UsageCheckResult, UsageError> {
        self.inner.check_can_use(organization_id).await
    }

    async fn get_balance(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<services::usage::OrganizationBalanceInfo>, UsageError> {
        self.inner.get_balance(organization_id).await
    }

    async fn get_usage_history(
        &self,
        organization_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<(Vec<services::usage::UsageLogEntry>, i64), UsageError> {
        self.inner
            .get_usage_history(organization_id, limit, offset)
            .await
    }

    async fn get_limit(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<services::usage::OrganizationLimit>, UsageError> {
        self.inner.get_limit(organization_id).await
    }

    async fn get_credit_limits(
        &self,
        organization_id: Uuid,
    ) -> Result<Vec<services::usage::OrganizationCreditLimit>, UsageError> {
        self.inner.get_credit_limits(organization_id).await
    }

    async fn get_usage_history_by_api_key(
        &self,
        api_key_id: Uuid,
        credit_type: Option<&str>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<(Vec<services::usage::UsageLogEntry>, i64), UsageError> {
        self.inner
            .get_usage_history_by_api_key(api_key_id, credit_type, limit, offset)
            .await
    }

    async fn get_api_key_usage_history_with_permissions(
        &self,
        workspace_id: Uuid,
        api_key_id: Uuid,
        user_id: Uuid,
        credit_type: Option<&str>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<(Vec<services::usage::UsageLogEntry>, i64), UsageError> {
        self.inner
            .get_api_key_usage_history_with_permissions(
                workspace_id,
                api_key_id,
                user_id,
                credit_type,
                limit,
                offset,
            )
            .await
    }

    async fn get_costs_by_inference_ids(
        &self,
        organization_id: Uuid,
        inference_ids: Vec<Uuid>,
    ) -> Result<Vec<services::usage::InferenceCost>, UsageError> {
        self.inner
            .get_costs_by_inference_ids(organization_id, inference_ids)
            .await
    }

    async fn get_usage_by_model(
        &self,
        organization_id: Uuid,
        start_date: chrono::DateTime<chrono::Utc>,
        end_date: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<services::usage::UsageByModelEntry>, UsageError> {
        self.inner
            .get_usage_by_model(organization_id, start_date, end_date)
            .await
    }

    async fn list_inference_usage_report(
        &self,
        query: services::usage::InferenceUsageReportQuery,
    ) -> Result<Vec<services::usage::InferenceUsageReportRow>, UsageError> {
        self.inner.list_inference_usage_report(query).await
    }

    async fn list_inference_usage_history(
        &self,
        query: services::usage::InferenceUsageHistoryQuery,
    ) -> Result<(Vec<services::usage::InferenceUsageReportRow>, i64), UsageError> {
        self.inner.list_inference_usage_history(query).await
    }
}

struct Fixture {
    server: axum_test::TestServer,
    recorder: Arc<RecordingUsageService>,
    org_id: Uuid,
    api_key: String,
}

/// Server whose AppState usage service is the recorder; `fail_next` injects
/// that many `record_usage` failures. `extra_models` are registered with the
/// mock provider (per-test model names).
async fn fixture(fail_next: usize, extra_models: &[&str]) -> Fixture {
    let mut recorder = None;
    let (server, _db) = setup_test_server_with_domain_services(|services, _db| {
        let wrapped = Arc::new(RecordingUsageService {
            inner: services.usage_service.clone(),
            records: Mutex::new(Vec::new()),
            fail_next: AtomicUsize::new(fail_next),
        });
        services.usage_service = wrapped.clone();
        recorder = Some((wrapped, services.inference_provider_pool.clone()));
    })
    .await;
    let (recorder, pool) = recorder.expect("domain services mutator should run");
    for model in extra_models {
        let provider: Arc<dyn inference_providers::InferenceProvider + Send + Sync> =
            Arc::new(inference_providers::mock::MockProvider::new_accept_all());
        pool.register_provider(model.to_string(), provider).await;
    }
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let org_id = Uuid::parse_str(&org.id).expect("org id should be a UUID");
    let api_key = get_api_key_for_org(&server, org.id).await;
    Fixture {
        server,
        recorder,
        org_id,
        api_key,
    }
}

/// Fields shared by every non-chat record: no cache, tiers, latency,
/// response link or discount; stop reason `Completed`.
fn assert_common_shape(record: &RecordUsageServiceRequest, org_id: Uuid) {
    assert_eq!(record.organization_id, org_id);
    assert_eq!(record.cache_read_tokens, 0);
    assert!(record.cache_write.is_none());
    assert_eq!(record.profiled_cache_write_tokens, 0);
    assert!(record.requested_service_tier.is_none());
    assert!(record.provider_service_tier.is_none());
    assert!(record.ttft_ms.is_none());
    assert!(record.avg_itl_ms.is_none());
    assert_eq!(record.stop_reason, Some(StopReason::Completed));
    assert!(record.response_id.is_none());
    assert!(record.discount.is_none());
}

fn assert_random_v4(record: &RecordUsageServiceRequest) {
    let id = record.inference_id.expect("inference_id should be set");
    assert_eq!(id.get_version_num(), 4, "inference_id should be random v4");
    assert!(record.provider_request_id.is_none());
}

fn assert_default_attribution(record: &RecordUsageServiceRequest) {
    assert_eq!(
        record.provider_attribution,
        services::usage::ProviderAttribution::default()
    );
}

fn assert_same_request(a: &RecordUsageServiceRequest, b: &RecordUsageServiceRequest) {
    assert_eq!(a, b, "retry must resend the identical request");
}

async fn post_image_generation(f: &Fixture) -> axum_test::TestResponse {
    f.server
        .post("/v1/images/generations")
        .add_header("Authorization", format!("Bearer {}", f.api_key))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": E2E_QWEN_IMAGE_MODEL_NAME,
            "prompt": "A sunset over mountains",
            "n": 1,
            "response_format": "b64_json"
        }))
        .await
}

fn test_png() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
}

async fn post_image_edit(f: &Fixture) -> axum_test::TestResponse {
    f.server
        .post("/v1/images/edits")
        .add_header("Authorization", format!("Bearer {}", f.api_key))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .multipart(
            axum_test::multipart::MultipartForm::new()
                .add_part(
                    "image",
                    axum_test::multipart::Part::bytes(test_png())
                        .file_name("image.png")
                        .mime_type("image/png"),
                )
                .add_text("model", E2E_QWEN_IMAGE_MODEL_NAME)
                .add_text("prompt", "Make it sunny"),
        )
        .await
}

async fn post_audio(f: &Fixture, model: &str) -> axum_test::TestResponse {
    f.server
        .post("/v1/audio/transcriptions")
        .add_header("Authorization", format!("Bearer {}", f.api_key))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .multipart(
            axum_test::multipart::MultipartForm::new()
                .add_part(
                    "file",
                    // 100 KB → mock duration 10.0s
                    axum_test::multipart::Part::bytes(vec![0u8; 100 * 1024])
                        .file_name("test.mp3")
                        .mime_type("audio/mpeg"),
                )
                .add_text("model", model),
        )
        .await
}

/// Register a test-owned model billed per input unit at $0.001.
async fn create_input_billed_model(
    f: &Fixture,
    model: &str,
    label: &str,
    context_length: u32,
    text_modalities: bool,
) {
    let mut config = serde_json::json!({
        "inputCostPerToken": {"amount": 1000000, "currency": "USD"},
        "outputCostPerToken": {"amount": 0, "currency": "USD"},
        "costPerImage": {"amount": 0, "currency": "USD"},
        "modelDisplayName": label,
        "modelDescription": label,
        "contextLength": context_length,
        "maxOutputLength": 1024,
        "verifiable": false,
        "isActive": true
    });
    if text_modalities {
        config["inputModalities"] = serde_json::json!(["text"]);
        config["outputModalities"] = serde_json::json!(["text"]);
    }
    let mut batch = api::models::BatchUpdateModelApiRequest::new();
    batch.insert(model.to_string(), serde_json::from_value(config).unwrap());
    admin_batch_upsert_models(&f.server, batch, get_session_id()).await;
}

async fn create_audio_model(f: &Fixture, model: &str) {
    create_input_billed_model(f, model, "Billing characterization audio", 4096, true).await;
}

async fn create_embedding_model(f: &Fixture, model: &str) {
    create_input_billed_model(f, model, "Billing characterization embedding", 32768, false).await;
}

async fn post_json(f: &Fixture, path: &str, body: serde_json::Value) -> axum_test::TestResponse {
    f.server
        .post(path)
        .add_header("Authorization", format!("Bearer {}", f.api_key))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await
}

fn rerank_body() -> serde_json::Value {
    serde_json::json!({
        "model": E2E_QWEN_RERANKER_MODEL_NAME,
        "query": "capital of France?",
        "documents": ["Brasilia is in Brazil.", "Paris is in France."]
    })
}

fn score_body() -> serde_json::Value {
    serde_json::json!({
        "model": E2E_QWEN_RERANKER_MODEL_NAME,
        "text_1": "capital of France?",
        "text_2": "Paris is in France."
    })
}

fn classify_body() -> serde_json::Value {
    serde_json::json!({
        "model": E2E_PRIVACY_FILTER_MODEL_NAME,
        "input": "My SSN is 123-45-6789",
        "threshold": 0.5
    })
}

fn redact_body() -> serde_json::Value {
    serde_json::json!({
        "model": E2E_PRIVACY_FILTER_MODEL_NAME,
        "input": "Email alice@example.com please"
    })
}

// ---------------------------------------------------------------- images

#[tokio::test]
async fn image_generation_records_hashed_id_with_image_count() {
    let f = fixture(0, &[]).await;
    let response = post_image_generation(&f).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let record = &f.recorder.wait_for(1).await[0];

    assert_common_shape(record, f.org_id);
    assert_eq!(record.inference_type, InferenceType::ImageGeneration);
    assert_eq!(record.input_tokens, 0);
    assert_eq!(record.output_tokens, 0);
    assert_eq!(record.image_count, Some(1));
    let provider_id = record
        .provider_request_id
        .clone()
        .expect("image records carry provider_request_id");
    assert_eq!(
        record.inference_id,
        Some(hash_inference_id_to_uuid(&provider_id))
    );
}

#[tokio::test]
async fn image_generation_falls_back_to_async_retry_on_record_failure() {
    let f = fixture(1, &[]).await;
    let response = post_image_generation(&f).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let records = f.recorder.wait_for(2).await;
    assert_same_request(&records[0], &records[1]);
}

#[tokio::test]
async fn image_edit_records_hashed_id_with_image_count() {
    let f = fixture(0, &[]).await;
    let response = post_image_edit(&f).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let record = &f.recorder.wait_for(1).await[0];

    assert_common_shape(record, f.org_id);
    assert_eq!(record.inference_type, InferenceType::ImageEdit);
    assert_eq!(record.input_tokens, 0);
    assert_eq!(record.output_tokens, 0);
    assert_eq!(record.image_count, Some(1));
    let provider_id = record
        .provider_request_id
        .clone()
        .expect("image records carry provider_request_id");
    assert_eq!(
        record.inference_id,
        Some(hash_inference_id_to_uuid(&provider_id))
    );
}

#[tokio::test]
async fn image_edit_falls_back_to_async_retry_on_record_failure() {
    let f = fixture(1, &[]).await;
    let response = post_image_edit(&f).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let records = f.recorder.wait_for(2).await;
    assert_same_request(&records[0], &records[1]);
}

// ---------------------------------------------------------------- audio

#[tokio::test]
async fn audio_records_duration_seconds_with_random_id() {
    let model = format!("billing-char/Whisper-{}", Uuid::new_v4());
    let f = fixture(0, &[&model]).await;
    create_audio_model(&f, &model).await;
    let response = post_audio(&f, &model).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let record = &f.recorder.wait_for(1).await[0];

    assert_common_shape(record, f.org_id);
    assert_eq!(record.inference_type, InferenceType::AudioTranscription);
    // MockProvider reports duration as file_size_kb * 0.1 s
    // (inference_providers/src/mock.rs): 100 KB → 10 s.
    assert_eq!(record.input_tokens, 10, "billed by ceil(duration seconds)");
    assert_eq!(record.output_tokens, 0);
    assert!(record.image_count.is_none());
    assert_random_v4(record);
    assert_default_attribution(record);
}

#[tokio::test]
async fn audio_fails_closed_on_record_failure() {
    let model = format!("billing-char/Whisper-{}", Uuid::new_v4());
    let f = fixture(1, &[&model]).await;
    create_audio_model(&f, &model).await;
    let response = post_audio(&f, &model).await;
    assert_eq!(response.status_code(), 500, "{}", response.text());
    f.recorder.wait_for(1).await;
}

// ---------------------------------------------------------------- rerank

#[tokio::test]
async fn rerank_records_total_tokens_with_random_id() {
    let f = fixture(0, &[]).await;
    let response = post_json(&f, "/v1/rerank", rerank_body()).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let record = &f.recorder.wait_for(1).await[0];

    assert_common_shape(record, f.org_id);
    assert_eq!(record.inference_type, InferenceType::Rerank);
    // MockProvider hard-codes rerank usage at 50 tokens
    // (inference_providers/src/mock.rs); update both together.
    assert_eq!(record.input_tokens, 50);
    assert_eq!(record.output_tokens, 0);
    assert!(record.image_count.is_none());
    assert_random_v4(record);
    assert_default_attribution(record);
}

#[tokio::test]
async fn rerank_fails_closed_on_record_failure() {
    let f = fixture(1, &[]).await;
    let response = post_json(&f, "/v1/rerank", rerank_body()).await;
    assert_eq!(response.status_code(), 500, "{}", response.text());
    f.recorder.wait_for(1).await;
}

// ---------------------------------------------------------------- embeddings

#[tokio::test]
async fn embeddings_records_total_tokens_with_random_id() {
    let model = format!("billing-char/Embedding-{}", Uuid::new_v4());
    let f = fixture(0, &[&model]).await;
    create_embedding_model(&f, &model).await;
    let response = post_json(
        &f,
        "/v1/embeddings",
        serde_json::json!({"model": model, "input": "Hello world"}),
    )
    .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let record = &f.recorder.wait_for(1).await[0];

    assert_common_shape(record, f.org_id);
    assert_eq!(record.inference_type, InferenceType::Embedding);
    // MockProvider always reports 10 embedding tokens
    // (inference_providers/src/mock.rs); update both together.
    assert_eq!(record.input_tokens, 10);
    assert_eq!(record.output_tokens, 0);
    assert!(record.image_count.is_none());
    assert_random_v4(record);
    assert_default_attribution(record);
}

#[tokio::test]
async fn embeddings_fails_closed_on_record_failure() {
    let model = format!("billing-char/Embedding-{}", Uuid::new_v4());
    let f = fixture(1, &[&model]).await;
    create_embedding_model(&f, &model).await;
    let response = post_json(
        &f,
        "/v1/embeddings",
        serde_json::json!({"model": model, "input": "Hello world"}),
    )
    .await;
    assert_eq!(response.status_code(), 500, "{}", response.text());
    f.recorder.wait_for(1).await;
}

// ---------------------------------------------------------------- score

#[tokio::test]
async fn score_records_one_token_under_hashed_response_id() {
    let f = fixture(0, &[]).await;
    let response = post_json(&f, "/v1/score", score_body()).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body: serde_json::Value = response.json();
    let response_id = body["id"].as_str().expect("score response has id");
    let record = &f.recorder.wait_for(1).await[0];

    assert_common_shape(record, f.org_id);
    assert_eq!(record.inference_type, InferenceType::Score);
    assert_eq!(record.input_tokens, 1, "score bills a flat single unit");
    assert_eq!(record.output_tokens, 0);
    assert!(record.image_count.is_none());
    assert_eq!(
        record.inference_id,
        Some(hash_inference_id_to_uuid(response_id))
    );
    assert!(record.provider_request_id.is_none());
    assert_default_attribution(record);
}

#[tokio::test]
async fn score_falls_back_to_async_retry_on_record_failure() {
    let f = fixture(1, &[]).await;
    let response = post_json(&f, "/v1/score", score_body()).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let records = f.recorder.wait_for(2).await;
    assert_same_request(&records[0], &records[1]);
}

// ---------------------------------------------------------------- privacy

#[tokio::test]
async fn privacy_classify_records_summed_tokens_with_random_id() {
    let f = fixture(0, &[]).await;
    let response = post_json(&f, "/v1/privacy/classify", classify_body()).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body: serde_json::Value = response.json();
    let expected: i64 = body["data"]
        .as_array()
        .expect("data array")
        .iter()
        // Mirror the route's summation rule (privacy.rs): skip entries without
        // usage and drop negative token counts before summing.
        .filter_map(|d| d["usage"]["input_tokens"].as_i64())
        .filter(|t| *t >= 0)
        .sum();
    let record = &f.recorder.wait_for(1).await[0];

    assert_common_shape(record, f.org_id);
    assert_eq!(record.inference_type, InferenceType::PrivacyClassify);
    assert!(expected > 0);
    assert_eq!(i64::from(record.input_tokens), expected);
    assert_eq!(record.output_tokens, 0);
    assert!(record.image_count.is_none());
    assert_random_v4(record);
    assert_default_attribution(record);
}

#[tokio::test]
async fn privacy_classify_falls_back_to_async_retry_on_record_failure() {
    let f = fixture(1, &[]).await;
    let response = post_json(&f, "/v1/privacy/classify", classify_body()).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let records = f.recorder.wait_for(2).await;
    assert_same_request(&records[0], &records[1]);
    assert_eq!(
        response.header("Inference-Id"),
        records[0].inference_id.unwrap().to_string()
    );
}

#[tokio::test]
async fn privacy_redact_records_as_classify_with_random_id() {
    let f = fixture(0, &[]).await;
    let response = post_json(&f, "/v1/privacy/redact", redact_body()).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let record = &f.recorder.wait_for(1).await[0];

    assert_common_shape(record, f.org_id);
    assert_eq!(record.inference_type, InferenceType::PrivacyClassify);
    assert!(record.input_tokens > 0);
    assert_eq!(record.output_tokens, 0);
    assert!(record.image_count.is_none());
    assert_random_v4(record);
    assert_default_attribution(record);
}

#[tokio::test]
async fn privacy_redact_falls_back_to_async_retry_on_record_failure() {
    let f = fixture(1, &[]).await;
    let response = post_json(&f, "/v1/privacy/redact", redact_body()).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let records = f.recorder.wait_for(2).await;
    assert_same_request(&records[0], &records[1]);
    assert_eq!(
        response.header("Inference-Id"),
        records[0].inference_id.unwrap().to_string()
    );
}

// ---------------------------------------------------------------- auto-redact

fn auto_redact_chat_body() -> serde_json::Value {
    serde_json::json!({
        "model": E2E_QWEN_MODEL_NAME,
        "messages": [{"role": "user", "content": "Please reach out to alice@example.com"}]
    })
}

async fn post_auto_redact_chat(f: &Fixture) -> axum_test::TestResponse {
    f.server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", f.api_key))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("x-auto-redact", "on")
        .json(&auto_redact_chat_body())
        .await
}

/// The chat completion itself bills through the completion service; the
/// AppState usage service sees only the auto-redact classify charge.
#[tokio::test]
async fn auto_redact_classify_records_against_pii_model_with_random_id() {
    let f = fixture(0, &[]).await;
    let response = post_auto_redact_chat(&f).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let record = &f.recorder.wait_for(1).await[0];

    assert_common_shape(record, f.org_id);
    assert_eq!(record.inference_type, InferenceType::PrivacyClassify);
    assert!(record.input_tokens > 0);
    assert_eq!(record.output_tokens, 0);
    assert!(record.image_count.is_none());
    assert_random_v4(record);
    assert_default_attribution(record);
}

#[tokio::test]
async fn auto_redact_classify_record_failure_is_swallowed_without_retry() {
    let f = fixture(1, &[]).await;
    let response = post_auto_redact_chat(&f).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    f.recorder.wait_for(1).await;
}

// ---------------------------------------------------------------- System One

async fn systemone_fixture(fail_next: usize) -> (Fixture, wiremock::MockServer, String) {
    use wiremock::matchers::{method, path};
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "jev-1.13.0",
                "answers": {"billing": {"type": "noul", "noul": 0.95}},
                "usage": {"input_tokens": 10, "output_tokens": 3}
            })),
        )
        .mount(&upstream)
        .await;
    let f = fixture(fail_next, &[]).await;
    let model = format!("typesafe/billing-char-{}", Uuid::new_v4());
    let mut batch = api::models::BatchUpdateModelApiRequest::new();
    batch.insert(
        model.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {"amount": 3, "currency": "USD"},
            "outputCostPerToken": {"amount": 2, "currency": "USD"},
            "modelDisplayName": "Billing characterization decisions",
            "modelDescription": "Billing characterization decisions",
            "contextLength": 32768,
            "maxOutputLength": 512,
            "verifiable": false,
            "isActive": true,
            "attestationSupported": false,
            "inputModalities": ["text"],
            "outputModalities": ["decisions"],
            "providerType": "external",
            "providerConfig": {
                "backend": "typesafe",
                "base_url": format!("{}/v1", upstream.uri()),
                "model_name": "jev-1.13.0",
                "api_key": "fixture-typesafe-key"
            }
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&f.server, batch, get_session_id()).await;
    (f, upstream, model)
}

async fn post_systemone(f: &Fixture, model: &str) -> axum_test::TestResponse {
    post_json(
        f,
        "/v1/systemone",
        serde_json::json!({
            "model": model,
            "state": {"message": "Please refund this charge"},
            "questions": {"billing": {"type": "noul", "instructions": "Is this about billing?"}}
        }),
    )
    .await
}

#[tokio::test]
async fn systemone_records_tokens_under_hashed_decision_id() {
    let (f, _upstream, model) = systemone_fixture(0).await;
    let response = post_systemone(&f, &model).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let decision_id = response
        .header("x-generation-id")
        .to_str()
        .expect("generation id header")
        .to_owned();
    let record = &f.recorder.wait_for(1).await[0];

    assert_common_shape(record, f.org_id);
    assert_eq!(record.inference_type, InferenceType::Decisions);
    assert_eq!(record.input_tokens, 10);
    assert_eq!(record.output_tokens, 3);
    assert!(record.image_count.is_none());
    assert_eq!(
        record.inference_id,
        Some(hash_inference_id_to_uuid(&decision_id))
    );
    assert_eq!(
        record.provider_request_id.as_deref(),
        Some(decision_id.as_str())
    );
}

#[tokio::test]
async fn systemone_falls_back_to_async_retry_on_record_failure() {
    let (f, _upstream, model) = systemone_fixture(1).await;
    let response = post_systemone(&f, &model).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let records = f.recorder.wait_for(2).await;
    assert_same_request(&records[0], &records[1]);
}
