use super::provider_attribution_tests::{
    completion_request, completion_service_with_mock_providers,
};
use super::usage_observer::{UsageFireOnce, UsageObserver, UsageReport};
use super::*;
use crate::metrics::capturing::CapturingMetricsService;
use crate::test_utils::{MockAttestationService, MockUsageService};
use bytes::Bytes;
use futures::{stream, StreamExt};
use inference_providers::models::{ChatChoice, ChatCompletionChunk, FinishReason, TokenUsage};
use std::sync::Mutex;

#[derive(Debug, Default)]
struct RecordingObserver {
    reports: Mutex<Vec<UsageReport>>,
}

impl RecordingObserver {
    fn reports(&self) -> Vec<UsageReport> {
        self.reports.lock().expect("observer lock").clone()
    }
}

impl UsageObserver for RecordingObserver {
    fn on_finished(&self, report: UsageReport) {
        self.reports.lock().expect("observer lock").push(report);
    }
}

fn chunk(usage: Option<TokenUsage>) -> SSEEvent {
    SSEEvent {
        raw_bytes: Bytes::from("data: ..."),
        raw_passthrough: true,
        chunk: Some(StreamChunk::Chat(ChatCompletionChunk {
            id: "chat-1".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "test-model".to_string(),
            choices: usage
                .as_ref()
                .map(|_| {
                    vec![ChatChoice {
                        index: 0,
                        delta: None,
                        logprobs: None,
                        finish_reason: Some(FinishReason::Stop),
                        token_ids: None,
                    }]
                })
                .unwrap_or_default(),
            usage,
            service_tier: None,
            prompt_token_ids: None,
            system_fingerprint: None,
            modality: None,
            extra: Default::default(),
        })),
    }
}

fn usage(prompt: i32, completion: i32) -> TokenUsage {
    TokenUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        ..Default::default()
    }
}

fn observed_stream(
    events: Vec<Result<SSEEvent, inference_providers::CompletionError>>,
    observer: Arc<RecordingObserver>,
    request_id: Uuid,
) -> InterceptStream<
    impl Stream<Item = Result<SSEEvent, inference_providers::CompletionError>> + Unpin,
> {
    let now = Instant::now();
    InterceptStream {
        inner: stream::iter(events),
        attestation_service: Arc::new(MockAttestationService),
        usage_service: Arc::new(MockUsageService),
        metrics_service: Arc::new(CapturingMetricsService::new()),
        request_id,
        organization_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
        api_key_id: Uuid::new_v4(),
        model_id: Uuid::new_v4(),
        model_name: "test-model".to_string(),
        inference_type: crate::usage::ports::InferenceType::ChatCompletionStream,
        service_start_time: now,
        provider_start_time: now,
        first_token_received: false,
        first_token_time: None,
        ttft_ms: None,
        token_count: 0,
        last_token_time: None,
        total_itl_ms: 0.0,
        metric_tags: CompletionServiceImpl::create_metric_tags("test-model"),
        concurrent_counter: None,
        last_usage_stats: None,
        last_chat_id: None,
        stream_completed: false,
        saw_upstream_done_marker: false,
        response_id: None,
        last_finish_reason: None,
        last_error: None,
        state: StreamState::Streaming,
        attestation_supported: true,
        store_provider_chat_signature: true,
        provider_attribution: crate::usage::ProviderAttribution::default(),
        cache_write_cost_per_token: None,
        requested_service_tier: None,
        provider_service_tier: None,
        latency_reporter: None,
        usage_observer: UsageFireOnce::new(Some(observer)),
    }
}

#[tokio::test]
async fn streaming_with_usage_reports_counts_once() {
    let observer = Arc::new(RecordingObserver::default());
    let request_id = Uuid::new_v4();
    let mut stream = observed_stream(
        vec![Ok(chunk(None)), Ok(chunk(Some(usage(11, 23))))],
        observer.clone(),
        request_id,
    );
    while stream.next().await.is_some() {}
    assert!(
        observer.reports().is_empty(),
        "must not fire before the stream is finished"
    );
    drop(stream);

    assert_eq!(
        observer.reports(),
        vec![UsageReport {
            request_id,
            prompt_tokens: 11,
            completion_tokens: 23,
            completed: true,
            output_started: true,
        }]
    );
}

#[tokio::test]
async fn dropped_stream_before_usage_reports_incomplete() {
    let observer = Arc::new(RecordingObserver::default());
    let request_id = Uuid::new_v4();
    let mut stream = observed_stream(
        vec![Ok(chunk(None)), Ok(chunk(Some(usage(11, 23))))],
        observer.clone(),
        request_id,
    );
    // Read only the first, usage-less chunk, then the client goes away.
    assert!(stream.next().await.is_some());
    drop(stream);

    assert_eq!(
        observer.reports(),
        vec![UsageReport {
            request_id,
            prompt_tokens: 0,
            completion_tokens: 0,
            completed: false,
            output_started: true,
        }]
    );
}

#[tokio::test]
async fn dropped_stream_after_partial_usage_is_incomplete() {
    let observer = Arc::new(RecordingObserver::default());
    let mut stream = observed_stream(
        vec![Ok(chunk(Some(usage(11, 2))))],
        observer.clone(),
        Uuid::new_v4(),
    );
    assert!(stream.next().await.is_some());
    drop(stream);
    let reports = observer.reports();
    assert_eq!(reports.len(), 1);
    assert!(!reports[0].completed);
    assert_eq!(
        (reports[0].prompt_tokens, reports[0].completion_tokens),
        (11, 2)
    );
}

#[tokio::test]
async fn streaming_service_propagates_observer_and_final_counts() {
    let model_name = "z-ai/glm-5.1";
    let (service, usage_service) =
        completion_service_with_mock_providers(model_name, false, false).await;
    let observer = Arc::new(RecordingObserver::default());
    let mut request = completion_request(model_name);
    let request_id = request.request_id;
    request.stream = Some(true);
    request.usage_observer = Some(observer.clone());
    let mut stream = service
        .create_chat_completion_stream(request)
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        event.unwrap();
    }
    drop(stream);
    let reports = observer.reports();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].request_id, request_id);
    assert!(reports[0].completed);
    let usage = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let usage = usage_service.get_requests();
            if !usage.is_empty() {
                break usage;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(
        (reports[0].prompt_tokens, reports[0].completion_tokens),
        (usage[0].input_tokens, usage[0].output_tokens)
    );
}

#[tokio::test]
async fn reaching_record_usage_twice_fires_once() {
    let observer = Arc::new(RecordingObserver::default());
    let mut stream = observed_stream(
        vec![Ok(chunk(None)), Ok(chunk(Some(usage(5, 7))))],
        observer.clone(),
        Uuid::new_v4(),
    );
    while stream.next().await.is_some() {}

    stream.record_usage_and_metrics();
    stream.record_usage_and_metrics();
    drop(stream); // Drop reaches it a third time.

    let reports = observer.reports();
    assert_eq!(reports.len(), 1);
    assert_eq!(
        (reports[0].prompt_tokens, reports[0].completion_tokens),
        (5, 7)
    );
    assert!(reports[0].completed);
}

#[tokio::test]
async fn non_streaming_reports_the_recorded_usage_once() {
    let model_name = "z-ai/glm-5.1";
    let (service, usage_service) =
        completion_service_with_mock_providers(model_name, false, false).await;
    let observer = Arc::new(RecordingObserver::default());
    let mut request = completion_request(model_name);
    let request_id = request.request_id;
    request.usage_observer = Some(observer.clone());

    service
        .create_chat_completion(request)
        .await
        .expect("mock provider should serve the request");

    let recorded = usage_service.get_requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        observer.reports(),
        vec![UsageReport {
            request_id,
            prompt_tokens: recorded[0].input_tokens,
            completion_tokens: recorded[0].output_tokens,
            completed: true,
            output_started: true,
        }]
    );
}

#[tokio::test]
async fn non_streaming_failure_reports_incomplete_once() {
    let model_name = "z-ai/glm-5.1";
    let (service, _usage_service) =
        completion_service_with_mock_providers(model_name, true, true).await;
    let observer = Arc::new(RecordingObserver::default());
    let mut request = completion_request(model_name);
    let request_id = request.request_id;
    request.usage_observer = Some(observer.clone());

    assert!(service.create_chat_completion(request).await.is_err());

    assert_eq!(
        observer.reports(),
        vec![UsageReport {
            request_id,
            prompt_tokens: 0,
            completion_tokens: 0,
            completed: false,
            output_started: false,
        }]
    );
}
