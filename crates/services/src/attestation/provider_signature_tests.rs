//! Provider signature batching must retain successful fetches when the other
//! algorithm fails or times out before the streaming finalization deadline.
use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use config::ExternalProvidersConfig;
use inference_providers::{self as ip, mock::MockProvider, InferenceProvider};

use super::{
    chat_signature_lifecycle_tests::{lifecycle_service, FailingRepository, RecordingRepository},
    ports::{AttestationRepository, AttestationServiceTrait},
    AttestationError, AttestationService, ChatSignature, SignatureKind,
    STREAM_SIGNATURE_STORE_TIMEOUT,
};
use crate::{
    inference_provider_pool::InferenceProviderPool,
    metrics::{
        capturing::{CapturingMetricsService, MetricValue},
        consts::*,
    },
};

#[derive(Clone, Copy)]
enum FetchBehavior {
    Success,
    FailSecond,
    StallSecond,
    StallFirst,
    SlowSecond,
}

struct ControlledProvider {
    inner: MockProvider,
    behavior: FetchBehavior,
}

#[async_trait]
impl InferenceProvider for ControlledProvider {
    async fn get_signature(
        &self,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<ip::ChatSignature, ip::CompletionError> {
        match (signing_algo.as_deref(), self.behavior) {
            (Some("ecdsa"), FetchBehavior::StallFirst)
            | (Some("ed25519"), FetchBehavior::StallSecond) => std::future::pending().await,
            (Some("ed25519"), FetchBehavior::FailSecond) => Err(
                ip::CompletionError::CompletionError("synthetic fetch failure".to_string()),
            ),
            (Some("ed25519"), FetchBehavior::SlowSecond) => {
                tokio::time::sleep(Duration::from_secs(6)).await;
                self.inner.get_signature(chat_id, signing_algo).await
            }
            _ => {
                // Spending part of the budget on ECDSA catches a timeout that
                // is accidentally restarted for each algorithm.
                if matches!(self.behavior, FetchBehavior::StallSecond) {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                self.inner.get_signature(chat_id, signing_algo).await
            }
        }
    }

    fn unpin_chat_connection(&self, chat_id: &str) {
        self.inner.unpin_chat_connection(chat_id);
    }
    async fn models(&self) -> Result<ip::models::ModelsResponse, ip::models::ListModelsError> {
        self.inner.models().await
    }

    async fn chat_completion_stream(
        &self,
        params: ip::ChatCompletionParams,
        request_hash: String,
    ) -> Result<ip::StreamingResult, ip::CompletionError> {
        self.inner
            .chat_completion_stream(params, request_hash)
            .await
    }

    async fn chat_completion(
        &self,
        params: ip::ChatCompletionParams,
        request_hash: String,
    ) -> Result<ip::ChatCompletionResponseWithBytes, ip::CompletionError> {
        self.inner.chat_completion(params, request_hash).await
    }

    async fn text_completion_stream(
        &self,
        params: ip::CompletionParams,
    ) -> Result<ip::StreamingResult, ip::CompletionError> {
        self.inner.text_completion_stream(params).await
    }

    async fn image_generation(
        &self,
        params: ip::ImageGenerationParams,
        request_hash: String,
    ) -> Result<ip::ImageGenerationResponseWithBytes, ip::ImageGenerationError> {
        self.inner.image_generation(params, request_hash).await
    }

    async fn image_edit(
        &self,
        params: Arc<ip::ImageEditParams>,
        request_hash: String,
    ) -> Result<ip::ImageEditResponseWithBytes, ip::ImageEditError> {
        self.inner.image_edit(params, request_hash).await
    }

    async fn score(
        &self,
        params: ip::ScoreParams,
        request_hash: String,
    ) -> Result<ip::ScoreResponse, ip::ScoreError> {
        self.inner.score(params, request_hash).await
    }

    async fn rerank(
        &self,
        params: ip::RerankParams,
    ) -> Result<ip::RerankResponse, ip::RerankError> {
        self.inner.rerank(params).await
    }

    async fn embeddings_raw(
        &self,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, ip::EmbeddingError> {
        self.inner.embeddings_raw(body, extra).await
    }

    async fn privacy_classify_raw(
        &self,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, ip::PrivacyClassifyError> {
        self.inner.privacy_classify_raw(body, extra).await
    }

    async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
    ) -> Result<serde_json::Map<String, serde_json::Value>, ip::models::AttestationError> {
        self.inner
            .get_attestation_report(
                model,
                signing_algo,
                nonce,
                signing_address,
                include_tls_fingerprint,
            )
            .await
    }

    async fn audio_transcription(
        &self,
        params: ip::AudioTranscriptionParams,
        request_hash: String,
    ) -> Result<ip::AudioTranscriptionResponse, ip::AudioTranscriptionError> {
        self.inner.audio_transcription(params, request_hash).await
    }
}

async fn provider_service(
    chat_id: &str,
    behavior: FetchBehavior,
    repository: Arc<dyn AttestationRepository + Send + Sync>,
) -> (
    AttestationService,
    Arc<ControlledProvider>,
    Arc<CapturingMetricsService>,
) {
    let pool = Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ));
    let provider = Arc::new(ControlledProvider {
        inner: MockProvider::new(),
        behavior,
    });
    pool.store_chat_id_mapping(chat_id.to_string(), provider.clone())
        .await;
    let mut service = lifecycle_service(repository, pool);
    let metrics = Arc::new(CapturingMetricsService::new());
    service.metrics_service = metrics.clone();
    (service, provider, metrics)
}

fn assert_failure_metric(metrics: &CapturingMetricsService, reason: &str) {
    let failures: Vec<_> = metrics
        .get_metrics()
        .into_iter()
        .filter(|metric| metric.name == METRIC_VERIFICATION_FAILURE)
        .collect();
    assert_eq!(failures.len(), 1);
    assert!(matches!(failures[0].value, MetricValue::Count(1)));
    assert!(failures[0].tags.contains(&format!("{TAG_REASON}:{reason}")));
    assert!(!metrics
        .get_metrics()
        .iter()
        .any(|metric| metric.name == METRIC_VERIFICATION_SUCCESS));
}

#[tokio::test]
async fn provider_success_stores_both_signatures_in_one_batch_and_unpins() {
    let chat_id = "chatcmpl-provider-batch-success";
    let repository = RecordingRepository::default();
    let (service, provider, metrics) = provider_service(
        chat_id,
        FetchBehavior::Success,
        Arc::new(repository.clone()),
    )
    .await;
    service
        .store_stream_chat_signature_from_provider(chat_id)
        .await
        .unwrap();
    assert_eq!(repository.batch_sizes(), vec![2]);
    let stored = repository.stored();
    assert_eq!(
        stored
            .iter()
            .map(|(_, sig)| sig.signing_algo.as_str())
            .collect::<Vec<_>>(),
        vec!["ecdsa", "ed25519"]
    );
    assert!(stored
        .iter()
        .all(|(id, sig)| id == chat_id && sig.signature_kind == Some(SignatureKind::ProviderTee)));
    assert_eq!(
        provider.inner.unpinned_chat_ids(),
        vec![chat_id.to_string()]
    );
    assert!(metrics
        .get_metrics()
        .iter()
        .any(|metric| metric.name == METRIC_VERIFICATION_SUCCESS
            && matches!(metric.value, MetricValue::Count(1))));
    assert!(!metrics
        .get_metrics()
        .iter()
        .any(|metric| metric.name == METRIC_VERIFICATION_FAILURE));
}

#[tokio::test]
async fn provider_second_fetch_error_preserves_first_signature_and_unpins() {
    let chat_id = "chatcmpl-provider-partial-error";
    let repository = RecordingRepository::default();
    let (service, provider, metrics) = provider_service(
        chat_id,
        FetchBehavior::FailSecond,
        Arc::new(repository.clone()),
    )
    .await;
    let result = service
        .store_stream_chat_signature_from_provider(chat_id)
        .await;
    assert!(matches!(result, Err(AttestationError::ProviderError(_))));
    assert_eq!(repository.batch_sizes(), vec![1]);
    assert_eq!(repository.stored()[0].1.signing_algo, "ecdsa");
    assert_eq!(
        provider.inner.unpinned_chat_ids(),
        vec![chat_id.to_string()]
    );
    assert_failure_metric(&metrics, REASON_INFERENCE_ERROR);
}

/// Make the flush consume part of its reserved budget, so a passing test
/// requires the write to finish before the actual five-second outer timeout.
struct DelayedRepository(RecordingRepository);

#[async_trait]
impl AttestationRepository for DelayedRepository {
    async fn add_chat_signature(
        &self,
        chat_id: &str,
        signature: ChatSignature,
    ) -> Result<(), AttestationError> {
        self.0.add_chat_signature(chat_id, signature).await
    }
    async fn add_chat_signatures(
        &self,
        chat_id: &str,
        signatures: Vec<ChatSignature>,
    ) -> Result<(), AttestationError> {
        tokio::time::sleep(Duration::from_millis(500)).await;
        self.0.add_chat_signatures(chat_id, signatures).await
    }
    async fn get_chat_signature(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<ChatSignature, AttestationError> {
        self.0.get_chat_signature(chat_id, signing_algo).await
    }
}

#[tokio::test(start_paused = true)]
async fn provider_stalled_second_fetch_flushes_first_before_stream_timeout() {
    let chat_id = "chatcmpl-provider-partial-timeout";
    let repository = RecordingRepository::default();
    let (service, provider, metrics) = provider_service(
        chat_id,
        FetchBehavior::StallSecond,
        Arc::new(DelayedRepository(repository.clone())),
    )
    .await;
    let started = tokio::time::Instant::now();
    // The same hard deadline used by InterceptStream must not cancel the
    // partial write, even after ECDSA has used two seconds of the fetch budget.
    let result = tokio::time::timeout(
        STREAM_SIGNATURE_STORE_TIMEOUT,
        service.store_stream_chat_signature_from_provider(chat_id),
    )
    .await
    .expect("partial signature must be flushed before stream finalization cancels it");
    assert!(
        matches!(result, Err(AttestationError::ProviderError(message)) if message.contains("Timed out") && message.contains("ed25519"))
    );
    assert_eq!(repository.batch_sizes(), vec![1]);
    assert_eq!(repository.stored()[0].1.signing_algo, "ecdsa");
    assert!(started.elapsed() >= Duration::from_millis(4500));
    assert!(started.elapsed() < STREAM_SIGNATURE_STORE_TIMEOUT);
    assert_eq!(
        provider.inner.unpinned_chat_ids(),
        vec![chat_id.to_string()]
    );
    assert_failure_metric(&metrics, REASON_INFERENCE_ERROR);
}

#[tokio::test(start_paused = true)]
async fn provider_stalled_first_fetch_unpins_without_an_empty_write() {
    let chat_id = "chatcmpl-provider-first-timeout";
    let repository = RecordingRepository::default();
    let (service, provider, metrics) = provider_service(
        chat_id,
        FetchBehavior::StallFirst,
        Arc::new(repository.clone()),
    )
    .await;
    let result = tokio::time::timeout(
        STREAM_SIGNATURE_STORE_TIMEOUT,
        service.store_stream_chat_signature_from_provider(chat_id),
    )
    .await
    .unwrap();
    assert!(
        matches!(result, Err(AttestationError::ProviderError(message)) if message.contains("Timed out"))
    );
    assert!(repository.batch_sizes().is_empty());
    assert!(repository.stored().is_empty());
    assert_eq!(
        provider.inner.unpinned_chat_ids(),
        vec![chat_id.to_string()]
    );
    assert_failure_metric(&metrics, REASON_INFERENCE_ERROR);
}

#[tokio::test]
async fn provider_repository_error_is_metriced_and_unpins() {
    let chat_id = "chatcmpl-provider-repository-error";
    let (service, provider, metrics) =
        provider_service(chat_id, FetchBehavior::Success, Arc::new(FailingRepository)).await;
    let result = service
        .store_stream_chat_signature_from_provider(chat_id)
        .await;
    assert!(matches!(result, Err(AttestationError::RepositoryError(_))));
    assert_eq!(
        provider.inner.unpinned_chat_ids(),
        vec![chat_id.to_string()]
    );
    assert_failure_metric(&metrics, REASON_REPOSITORY_ERROR);
}

#[tokio::test(start_paused = true)]
async fn background_provider_fetch_keeps_its_existing_timeout() {
    let chat_id = "chatcmpl-provider-background-slow";
    let repository = RecordingRepository::default();
    let (service, provider, metrics) = provider_service(
        chat_id,
        FetchBehavior::SlowSecond,
        Arc::new(repository.clone()),
    )
    .await;
    let started = tokio::time::Instant::now();
    service
        .store_chat_signature_from_provider(chat_id)
        .await
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(6));
    assert_eq!(repository.batch_sizes(), vec![2]);
    assert_eq!(repository.stored().len(), 2);
    assert_eq!(
        provider.inner.unpinned_chat_ids(),
        vec![chat_id.to_string()]
    );
    assert!(metrics
        .get_metrics()
        .iter()
        .any(|metric| metric.name == METRIC_VERIFICATION_SUCCESS));
    assert!(!metrics
        .get_metrics()
        .iter()
        .any(|metric| metric.name == METRIC_VERIFICATION_FAILURE));
}
