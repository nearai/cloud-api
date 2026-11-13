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
    models::{ChatSignature, SignatureKind, SignatureLookupResult, SignatureOwner},
    ports::{AttestationRepository, AttestationServiceTrait},
    AttestationError, AttestationService, DstackGatewayQuoteCollector,
};
use crate::{
    inference_provider_pool::InferenceProviderPool,
    metrics::MetricsServiceTrait,
    models::{ModelWithPricing, ModelsRepository},
    usage::{
        InferenceCost, InferenceUsageHistoryQuery, InferenceUsageReportRow,
        OrganizationBalanceInfo, RecordUsageDbRequest, UsageByModelEntry, UsageLogEntry,
        UsageRepository,
    },
};

/// One stored row. `owner` is `None` for a row stored before owners were
/// recorded.
pub(super) type StoredRow = (Option<SignatureOwner>, String, ChatSignature);

/// The workspace the tests sign for.
pub(super) fn owner() -> SignatureOwner {
    SignatureOwner {
        organization_id: Uuid::from_u128(1),
        workspace_id: Uuid::from_u128(2),
    }
}

/// A workspace that did not produce the signature.
fn other_owner() -> SignatureOwner {
    SignatureOwner {
        organization_id: Uuid::from_u128(3),
        workspace_id: Uuid::from_u128(4),
    }
}

/// Repository that records stored signatures and follows the database's owner
/// rules: a row is keyed by id and algorithm, only its owner may replace it,
/// and any caller can read it, including a row with no owner.
#[derive(Clone, Default)]
pub(super) struct RecordingRepository {
    stored: Arc<Mutex<Vec<StoredRow>>>,
    batch_sizes: Arc<Mutex<Vec<usize>>>,
}

impl RecordingRepository {
    pub(super) fn stored(&self) -> Vec<(String, ChatSignature)> {
        self.stored_rows()
            .into_iter()
            .map(|(_, chat_id, signature)| (chat_id, signature))
            .collect()
    }

    pub(super) fn stored_rows(&self) -> Vec<StoredRow> {
        self.stored.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub(super) fn batch_sizes(&self) -> Vec<usize> {
        self.batch_sizes.lock().unwrap().clone()
    }

    /// Seed a row written before owners were recorded.
    pub(super) fn insert_legacy(&self, chat_id: &str, signature: ChatSignature) {
        self.stored
            .lock()
            .unwrap()
            .push((None, chat_id.to_string(), signature));
    }
}

#[async_trait]
impl AttestationRepository for RecordingRepository {
    async fn add_chat_signatures(
        &self,
        owner: SignatureOwner,
        chat_id: &str,
        signatures: Vec<ChatSignature>,
    ) -> Result<(), AttestationError> {
        self.batch_sizes.lock().unwrap().push(signatures.len());
        for signature in signatures {
            self.add_chat_signature(owner, chat_id, signature).await?;
        }
        Ok(())
    }

    async fn add_chat_signature(
        &self,
        owner: SignatureOwner,
        chat_id: &str,
        signature: ChatSignature,
    ) -> Result<(), AttestationError> {
        let mut stored = self
            .stored
            .lock()
            .map_err(|_| AttestationError::InternalError("stored lock poisoned".to_string()))?;
        match stored.iter_mut().find(|(_, stored_chat_id, existing)| {
            stored_chat_id == chat_id && existing.signing_algo == signature.signing_algo
        }) {
            Some((Some(existing_owner), _, existing)) if *existing_owner == owner => {
                *existing = signature;
                Ok(())
            }
            Some(_) => Err(AttestationError::OwnerConflict),
            None => {
                stored.push((Some(owner), chat_id.to_string(), signature));
                Ok(())
            }
        }
    }

    async fn get_chat_signature(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<ChatSignature, AttestationError> {
        self.stored
            .lock()
            .map_err(|_| AttestationError::InternalError("stored lock poisoned".to_string()))?
            .iter()
            .find(|(_, stored_chat_id, signature)| {
                stored_chat_id == chat_id && signature.signing_algo == signing_algo
            })
            .map(|(_, _, signature)| signature.clone())
            .ok_or_else(|| AttestationError::SignatureNotFound(format!("{chat_id}:{signing_algo}")))
    }
}

/// Repository whose store always fails.
pub(super) struct FailingRepository;

#[async_trait]
impl AttestationRepository for FailingRepository {
    async fn add_chat_signature(
        &self,
        _owner: SignatureOwner,
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
        _owner: SignatureOwner,
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
        _credit_type: Option<&str>,
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

    async fn get_usage_by_model(
        &self,
        _organization_id: Uuid,
        _start_date: chrono::DateTime<chrono::Utc>,
        _end_date: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<Vec<UsageByModelEntry>> {
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

pub(super) fn lifecycle_service(
    repository: Arc<dyn AttestationRepository + Send + Sync>,
    pool: Arc<InferenceProviderPool>,
) -> AttestationService {
    let (ed25519_signing_key, ed25519_verifying_key, ecdsa_signing_key, ecdsa_verifying_key) =
        AttestationService::generate_ephemeral_signing_keys().expect("ephemeral test keys");
    AttestationService {
        repository,
        inference_provider_pool: pool.clone(),
        models_repository: Arc::new(EmptyModelsRepository),
        model_resolve_cache: crate::models::new_model_resolve_cache(),
        metrics_service: Arc::new(NoopMetricsService),
        usage_repository: Arc::new(NoopUsageRepository),
        vpc_info: None,
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
        .store_chat_signature_and_unpin(
            owner(),
            chat_id,
            "req-hash".to_string(),
            "resp-hash".to_string(),
        )
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
async fn store_and_unpin_rejects_empty_id_without_storing_and_releases_pin() {
    let (pool, provider) = pool_with_pinned_chat("").await;
    let repository = RecordingRepository::default();
    let service = lifecycle_service(Arc::new(repository.clone()), pool);

    let result = service
        .store_chat_signature_and_unpin(
            owner(),
            "",
            "req-hash".to_string(),
            "resp-hash".to_string(),
        )
        .await;

    assert!(matches!(result, Err(AttestationError::InvalidParameter(_))));
    assert!(repository.stored().is_empty());
    assert!(repository.batch_sizes().is_empty());
    assert_eq!(provider.unpinned_chat_ids(), vec![String::new()]);
}

#[tokio::test]
async fn store_and_unpin_releases_pin_when_store_fails() {
    let chat_id = "chatcmpl-lifecycle-store-error";
    let (pool, provider) = pool_with_pinned_chat(chat_id).await;
    let service = lifecycle_service(Arc::new(FailingRepository), pool);

    let result = service
        .store_chat_signature_and_unpin(
            owner(),
            chat_id,
            "req-hash".to_string(),
            "resp-hash".to_string(),
        )
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
        .store_chat_signature_and_unpin(
            owner(),
            chat_id,
            "req-hash".to_string(),
            "resp-hash".to_string(),
        )
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
async fn unsupported_provider_releases_pin_without_storing_a_signature() {
    let chat_id = "chatcmpl-lifecycle-unsupported-provider";
    let pool = Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ));
    let provider = Arc::new(MockProvider::new().with_chat_signature_support(false));
    pool.store_chat_id_mapping(chat_id.to_string(), provider.clone())
        .await;
    let repository = RecordingRepository::default();
    let service = lifecycle_service(Arc::new(repository.clone()), pool);

    let result = service
        .store_chat_signature_from_provider(owner(), chat_id)
        .await;

    assert!(
        result.is_ok(),
        "unsupported providers should not fail completion"
    );
    assert!(
        repository.stored().is_empty(),
        "unsupported providers must not synthesize a provider signature"
    );
    assert_eq!(
        provider.unpinned_chat_ids(),
        vec![chat_id.to_string()],
        "the signature-fetch routing pin must be released exactly once"
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

fn service_without_pins(repository: &RecordingRepository) -> AttestationService {
    let pool = Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ));
    lifecycle_service(Arc::new(repository.clone()), pool)
}

async fn store_gateway(service: &AttestationService, owner: SignatureOwner, chat_id: &str) {
    service
        .store_chat_signature(
            owner,
            chat_id,
            "req-hash".to_string(),
            "resp-hash".to_string(),
        )
        .await
        .expect("gateway signature stored");
}

fn assert_not_found(result: Result<SignatureLookupResult, AttestationError>) {
    match result {
        Err(AttestationError::SignatureNotFound(_)) => {}
        other => panic!("expected SignatureNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn stored_signature_records_the_writing_owner() {
    let chat_id = "chatcmpl-owner-persisted";
    let repository = RecordingRepository::default();
    let service = service_without_pins(&repository);

    store_gateway(&service, owner(), chat_id).await;
    service
        .store_response_signature(owner(), "resp_owner", "a".to_string(), "b".to_string())
        .await
        .expect("response signature stored");

    let rows = repository.stored_rows();
    assert_eq!(rows.len(), 4, "two algorithms for each id");
    assert!(
        rows.iter()
            .all(|(row_owner, _, _)| *row_owner == Some(owner())),
        "every row must carry the owner it was written for"
    );
}

#[tokio::test]
async fn owner_can_read_its_signature() {
    let chat_id = "chatcmpl-owner-read";
    let repository = RecordingRepository::default();
    let service = service_without_pins(&repository);
    store_gateway(&service, owner(), chat_id).await;

    for algo in ["ecdsa", "ed25519"] {
        match service
            .get_chat_signature(owner(), chat_id, Some(algo.to_string()))
            .await
        {
            Ok(SignatureLookupResult::Found(signature)) => {
                assert_eq!(signature.signing_algo, algo);
                assert_eq!(signature.text, "req-hash:resp-hash");
            }
            other => panic!("expected the owner's signature, got {other:?}"),
        }
    }
}

fn assert_found_text(result: Result<SignatureLookupResult, AttestationError>, text: &str) {
    match result {
        Ok(SignatureLookupResult::Found(signature)) => assert_eq!(signature.text, text),
        other => panic!("expected the stored signature, got {other:?}"),
    }
}

/// Signatures are public verification material: any workspace holding the id
/// can read one, whichever workspace stored it.
#[tokio::test]
async fn another_owner_can_read_the_signature() {
    let chat_id = "chatcmpl-owner-other-read";
    let repository = RecordingRepository::default();
    let service = service_without_pins(&repository);
    store_gateway(&service, owner(), chat_id).await;

    assert_found_text(
        service
            .get_chat_signature(other_owner(), chat_id, None)
            .await,
        "req-hash:resp-hash",
    );
    // Same organization, different workspace.
    let sibling_workspace = SignatureOwner {
        workspace_id: Uuid::from_u128(99),
        ..owner()
    };
    assert_found_text(
        service
            .get_chat_signature(sibling_workspace, chat_id, Some("ed25519".to_string()))
            .await,
        "req-hash:resp-hash",
    );
}

#[tokio::test]
async fn legacy_signature_without_owner_is_readable() {
    let chat_id = "chatcmpl-owner-legacy";
    let repository = RecordingRepository::default();
    repository.insert_legacy(
        chat_id,
        ChatSignature {
            text: "legacy".to_string(),
            signature: "0xlegacy".to_string(),
            signing_address: "0xaddr".to_string(),
            signing_algo: "ecdsa".to_string(),
            signature_kind: None,
        },
    );
    let service = service_without_pins(&repository);

    assert_found_text(
        service.get_chat_signature(owner(), chat_id, None).await,
        "legacy",
    );
    assert_found_text(
        service
            .get_chat_signature(other_owner(), chat_id, None)
            .await,
        "legacy",
    );
}

#[tokio::test]
async fn another_owner_cannot_replace_a_stored_signature() {
    let chat_id = "chatcmpl-owner-overwrite";
    let repository = RecordingRepository::default();
    let service = service_without_pins(&repository);
    store_gateway(&service, owner(), chat_id).await;

    let result = service
        .store_chat_signature(
            other_owner(),
            chat_id,
            "other-req".to_string(),
            "other-resp".to_string(),
        )
        .await;

    assert!(matches!(result, Err(AttestationError::OwnerConflict)));
    let rows = repository.stored_rows();
    assert_eq!(rows.len(), 2);
    for (row_owner, _, signature) in rows {
        assert_eq!(row_owner, Some(owner()), "ownership must not move");
        assert_eq!(signature.text, "req-hash:resp-hash", "content unchanged");
    }
    // The other owner still reads the original signature, not its own.
    assert_found_text(
        service
            .get_chat_signature(other_owner(), chat_id, None)
            .await,
        "req-hash:resp-hash",
    );
}

#[tokio::test]
async fn miss_is_not_found_even_with_a_pinned_provider_without_signatures() {
    let chat_id = "chatcmpl-owner-unsupported";
    let pool = Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ));
    let provider = Arc::new(MockProvider::new().with_chat_signature_support(false));
    pool.store_chat_id_mapping(chat_id.to_string(), provider)
        .await;
    let service = lifecycle_service(Arc::new(RecordingRepository::default()), pool);

    assert_not_found(
        service
            .get_chat_signature(other_owner(), chat_id, None)
            .await,
    );
}
