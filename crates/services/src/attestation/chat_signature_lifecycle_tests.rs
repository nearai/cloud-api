//! Lifecycle tests for the gateway signature store+unpin path.
//!
//! `store_chat_signature_and_unpin` owns the same lifecycle contract as
//! `store_chat_signature_from_provider`: the provider-pool signature-fetch
//! routing pin must be released whether the store succeeds, fails, or times
//! out, and `release_chat_signature_pin` must release it without storing
//! anything (errored-stream path). These tests construct the service directly
//! (private-field struct literals are legal inside `crate::attestation`) with
//! a real `InferenceProviderPool` seeded via `store_chat_id_mapping` and a
//! `MockProvider` that records unpin calls.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use config::{ExternalProvidersConfig, ItaAttestationConfig};
use inference_providers::mock::MockProvider;
use uuid::Uuid;

use super::{
    chat_signatures::STREAM_SIGNATURE_STORE_TIMEOUT,
    ita::ProviderPoolModelAttestationCollector,
    models::{ChatSignature, SignatureKind},
    ports::{AttestationRepository, AttestationServiceTrait},
    AttestationError, AttestationService, DstackGatewayQuoteCollector,
};
use crate::{
    inference_provider_pool::InferenceProviderPool,
    metrics::MetricsServiceTrait,
    models::{ModelWithPricing, ModelsRepository},
    usage::{
        InferenceCost, InferenceUsageHistoryQuery, InferenceUsageReportQuery,
        InferenceUsageReportRow, OrganizationBalanceInfo, RecordUsageDbRequest, StopReason,
        UsageByModelEntry, UsageLogEntry, UsageRepository,
    },
};

/// Repository that records stored signatures and succeeds.
#[derive(Clone, Default)]
struct RecordingRepository {
    stored: Arc<Mutex<Vec<(String, ChatSignature)>>>,
}

impl RecordingRepository {
    fn stored(&self) -> Vec<(String, ChatSignature)> {
        self.stored.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl AttestationRepository for RecordingRepository {
    async fn add_chat_signature(
        &self,
        chat_id: &str,
        signature: ChatSignature,
    ) -> Result<(), AttestationError> {
        self.stored
            .lock()
            .map_err(|_| AttestationError::InternalError("stored lock poisoned".to_string()))?
            .push((chat_id.to_string(), signature));
        Ok(())
    }

    async fn get_chat_signature(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<ChatSignature, AttestationError> {
        Err(AttestationError::SignatureNotFound(format!(
            "{chat_id}:{signing_algo}"
        )))
    }
}

/// Repository whose store always fails.
struct FailingRepository;

#[async_trait]
impl AttestationRepository for FailingRepository {
    async fn add_chat_signature(
        &self,
        _chat_id: &str,
        _signature: ChatSignature,
    ) -> Result<(), AttestationError> {
        Err(AttestationError::RepositoryError(
            "simulated store failure".to_string(),
        ))
    }

    async fn get_chat_signature(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<ChatSignature, AttestationError> {
        Err(AttestationError::SignatureNotFound(format!(
            "{chat_id}:{signing_algo}"
        )))
    }
}

/// Repository whose store never completes (exercises the store timeout).
struct HangingRepository;

#[async_trait]
impl AttestationRepository for HangingRepository {
    async fn add_chat_signature(
        &self,
        _chat_id: &str,
        _signature: ChatSignature,
    ) -> Result<(), AttestationError> {
        std::future::pending().await
    }

    async fn get_chat_signature(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<ChatSignature, AttestationError> {
        Err(AttestationError::SignatureNotFound(format!(
            "{chat_id}:{signing_algo}"
        )))
    }
}

struct EmptyModelsRepository;

#[async_trait]
impl ModelsRepository for EmptyModelsRepository {
    async fn get_all_active_models(&self) -> anyhow::Result<Vec<ModelWithPricing>> {
        Ok(Vec::new())
    }

    async fn get_model_by_name(
        &self,
        _model_name: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok(None)
    }

    async fn resolve_and_get_model(
        &self,
        _identifier: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok(None)
    }

    async fn get_configured_model_names(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

struct NoopMetricsService;

impl MetricsServiceTrait for NoopMetricsService {
    fn record_latency(&self, _name: &str, _duration: std::time::Duration, _tags: &[&str]) {}
    fn record_count(&self, _name: &str, _value: i64, _tags: &[&str]) {}
    fn record_histogram(&self, _name: &str, _value: f64, _tags: &[&str]) {}
}

struct NoopUsageRepository;

#[async_trait]
impl UsageRepository for NoopUsageRepository {
    async fn record_usage(&self, _request: RecordUsageDbRequest) -> anyhow::Result<UsageLogEntry> {
        anyhow::bail!("unused")
    }

    async fn get_balance(
        &self,
        _organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationBalanceInfo>> {
        Ok(None)
    }

    async fn get_usage_history(
        &self,
        _organization_id: Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        Ok((Vec::new(), 0))
    }

    async fn get_usage_history_by_api_key(
        &self,
        _api_key_id: Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        Ok((Vec::new(), 0))
    }

    async fn get_api_key_spend(&self, _api_key_id: Uuid) -> anyhow::Result<i64> {
        Ok(0)
    }

    async fn get_costs_by_inference_ids(
        &self,
        _organization_id: Uuid,
        _inference_ids: Vec<Uuid>,
    ) -> anyhow::Result<Vec<InferenceCost>> {
        Ok(Vec::new())
    }

    async fn get_stop_reason_by_response_id(
        &self,
        _response_id: Uuid,
    ) -> anyhow::Result<Option<StopReason>> {
        Ok(None)
    }

    async fn get_stop_reason_by_provider_request_id(
        &self,
        _provider_request_id: &str,
    ) -> anyhow::Result<Option<StopReason>> {
        Ok(None)
    }

    async fn get_usage_by_model(
        &self,
        _organization_id: Uuid,
        _start_date: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<Vec<UsageByModelEntry>> {
        Ok(Vec::new())
    }

    async fn list_inference_usage_report(
        &self,
        _query: InferenceUsageReportQuery,
    ) -> anyhow::Result<Vec<InferenceUsageReportRow>> {
        Ok(Vec::new())
    }

    async fn list_inference_usage_history(
        &self,
        _query: InferenceUsageHistoryQuery,
    ) -> anyhow::Result<(Vec<InferenceUsageReportRow>, i64)> {
        Ok((Vec::new(), 0))
    }
}

/// Pool with a chat_id already pinned to a recording `MockProvider`, matching
/// the state a gateway-signed stream is in when its end-of-stream tail runs.
async fn pool_with_pinned_chat(chat_id: &str) -> (Arc<InferenceProviderPool>, Arc<MockProvider>) {
    let pool = Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ));
    let provider = Arc::new(MockProvider::new());
    pool.store_chat_id_mapping(chat_id.to_string(), provider.clone())
        .await;
    (pool, provider)
}

fn lifecycle_service(
    repository: Arc<dyn AttestationRepository + Send + Sync>,
    pool: Arc<InferenceProviderPool>,
) -> AttestationService {
    let (ed25519_signing_key, ed25519_verifying_key, ecdsa_signing_key, ecdsa_verifying_key) =
        AttestationService::generate_ephemeral_signing_keys();
    AttestationService {
        repository,
        inference_provider_pool: pool.clone(),
        models_repository: Arc::new(EmptyModelsRepository),
        metrics_service: Arc::new(NoopMetricsService),
        usage_repository: Arc::new(NoopUsageRepository),
        vpc_info: None,
        vpc_shared_secret: None,
        tls_cert_fingerprint: None,
        ed25519_signing_key: Arc::new(ed25519_signing_key),
        ed25519_verifying_key: Arc::new(ed25519_verifying_key),
        ecdsa_signing_key: Arc::new(ecdsa_signing_key),
        ecdsa_verifying_key: Arc::new(ecdsa_verifying_key),
        ita_config: ItaAttestationConfig::default(),
        ita_client: None,
        gateway_quote_collector: Arc::new(DstackGatewayQuoteCollector),
        model_attestation_collector: Arc::new(ProviderPoolModelAttestationCollector::new(pool)),
        report_cache: None,
    }
}

#[tokio::test]
async fn store_and_unpin_stores_gateway_signature_and_releases_pin_on_success() {
    let chat_id = "chatcmpl-lifecycle-success";
    let (pool, provider) = pool_with_pinned_chat(chat_id).await;
    let repository = RecordingRepository::default();
    let service = lifecycle_service(Arc::new(repository.clone()), pool);

    let result = service
        .store_chat_signature_and_unpin(chat_id, "req-hash".to_string(), "resp-hash".to_string())
        .await;

    assert!(result.is_ok(), "store should succeed: {result:?}");
    let stored = repository.stored();
    assert_eq!(stored.len(), 2, "one signature per signing algorithm");
    for (stored_chat_id, signature) in &stored {
        assert_eq!(stored_chat_id, chat_id);
        assert_eq!(signature.text, "req-hash:resp-hash");
        assert_eq!(signature.signature_kind, Some(SignatureKind::Gateway));
    }
    assert_eq!(
        provider.unpinned_chat_ids(),
        vec![chat_id.to_string()],
        "the signature-fetch routing pin must be released exactly once"
    );
}

#[tokio::test]
async fn store_and_unpin_releases_pin_when_store_fails() {
    let chat_id = "chatcmpl-lifecycle-store-error";
    let (pool, provider) = pool_with_pinned_chat(chat_id).await;
    let service = lifecycle_service(Arc::new(FailingRepository), pool);

    let result = service
        .store_chat_signature_and_unpin(chat_id, "req-hash".to_string(), "resp-hash".to_string())
        .await;

    assert!(result.is_err(), "store failure must propagate");
    assert_eq!(
        provider.unpinned_chat_ids(),
        vec![chat_id.to_string()],
        "the pin must be released even when the store fails"
    );
}

// `start_paused` lets the 5s store timeout elapse instantly: tokio auto-
// advances the paused clock once the hanging store is the only pending work.
#[tokio::test(start_paused = true)]
async fn store_and_unpin_releases_pin_when_store_times_out() {
    let chat_id = "chatcmpl-lifecycle-timeout";
    let (pool, provider) = pool_with_pinned_chat(chat_id).await;
    let service = lifecycle_service(Arc::new(HangingRepository), pool);

    let started = tokio::time::Instant::now();
    let result = service
        .store_chat_signature_and_unpin(chat_id, "req-hash".to_string(), "resp-hash".to_string())
        .await;

    assert!(
        started.elapsed() >= STREAM_SIGNATURE_STORE_TIMEOUT,
        "the hanging store must be cut off by the timeout, not complete"
    );
    match result {
        Err(AttestationError::InternalError(message)) => {
            assert!(
                message.contains("Timed out"),
                "timeout must surface as such: {message}"
            );
        }
        other => panic!("expected timeout error, got {other:?}"),
    }
    assert_eq!(
        provider.unpinned_chat_ids(),
        vec![chat_id.to_string()],
        "the pin must be released even when the store times out"
    );
}

#[tokio::test]
async fn release_chat_signature_pin_unpins_without_storing() {
    let chat_id = "chatcmpl-lifecycle-errored-stream";
    let (pool, provider) = pool_with_pinned_chat(chat_id).await;
    let repository = RecordingRepository::default();
    let service = lifecycle_service(Arc::new(repository.clone()), pool);

    service.release_chat_signature_pin(chat_id).await;

    assert!(
        repository.stored().is_empty(),
        "the errored-stream path must not store a signature"
    );
    assert_eq!(
        provider.unpinned_chat_ids(),
        vec![chat_id.to_string()],
        "the pin must still be released for errored streams"
    );
}

#[tokio::test]
async fn release_chat_signature_pin_is_a_noop_without_a_mapping() {
    let pool = Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ));
    let repository = RecordingRepository::default();
    let service = lifecycle_service(Arc::new(repository.clone()), pool);

    // No mapping for this chat_id — must not panic or store anything.
    service.release_chat_signature_pin("chatcmpl-unknown").await;
    assert!(repository.stored().is_empty());
}
