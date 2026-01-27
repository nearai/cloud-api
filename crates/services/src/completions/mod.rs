pub mod ports;

use crate::attestation::ports::AttestationServiceTrait;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::models::ModelsRepository;
use crate::responses::models::ResponseId;
use crate::usage::{RecordUsageServiceRequest, UsageServiceTrait};
use inference_providers::{
    ChatMessage, MessageRole, SSEEvent, ScoreError, StreamChunk, StreamingResult,
};
use moka::future::Cache;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use uuid::Uuid;

// Create a new stream that intercepts messages, but passes the original ones through
use crate::metrics::{consts::*, MetricsServiceTrait};
use futures_util::{Future, Stream};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

const FINALIZE_TIMEOUT_SECS: u64 = 5;

type FinalizeFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

enum StreamState {
    Streaming,
    Finalizing(FinalizeFuture),
    Done,
}

/// Hash inference ID to UUID deterministically using MD5 (v5)
/// Takes the full ID including prefix (e.g., "chatcmpl-abc123") and returns a stable UUID
pub fn hash_inference_id_to_uuid(full_id: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, full_id.as_bytes())
}

/// Get input bucket tag based on token count for metrics breakdown
/// Buckets: 0-1k, 1-4k, 4-16k, 16-32k, 32-64k, 64-128k, 128k+
fn get_input_bucket(token_count: i32) -> &'static str {
    match token_count {
        0..=1000 => "0-1k",
        1001..=4000 => "1-4k",
        4001..=16000 => "4-16k",
        16001..=32000 => "16-32k",
        32001..=64000 => "32-64k",
        64001..=128000 => "64-128k",
        _ => "128k+",
    }
}

struct InterceptStream<S>
where
    S: Stream<Item = Result<SSEEvent, inference_providers::CompletionError>> + Unpin,
{
    inner: S,
    attestation_service: Arc<dyn AttestationServiceTrait>,
    usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
    metrics_service: Arc<dyn MetricsServiceTrait>,
    // IDs for usage tracking (database)
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model_id: Uuid,
    #[allow(dead_code)] // Kept for potential debugging/logging use
    model_name: String,
    inference_type: String,
    service_start_time: Instant,
    provider_start_time: Instant,
    first_token_received: bool,
    first_token_time: Option<Instant>,
    /// Time to first token in milliseconds (captured for DB storage)
    ttft_ms: Option<i32>,
    /// Token count for ITL calculation
    token_count: i32,
    /// Last token time for ITL calculation
    last_token_time: Option<Instant>,
    /// Accumulated inter-token latency for average calculation
    total_itl_ms: f64,
    // Pre-allocated low-cardinality metric tags (for Datadog/OTLP)
    metric_tags: Vec<String>,
    concurrent_counter: Option<Arc<AtomicU32>>,
    /// Last received usage stats from streaming chunks
    last_usage_stats: Option<inference_providers::TokenUsage>,
    /// Last chat ID from streaming chunks (for attestation and inference_id)
    last_chat_id: Option<String>,
    /// Flag indicating the stream completed normally (received None from inner stream)
    /// If false when Drop is called, the client disconnected mid-stream
    stream_completed: bool,
    /// Response ID when called from Responses API (for usage tracking FK)
    response_id: Option<ResponseId>,
    /// Last finish_reason from provider (e.g., "stop", "length", "tool_calls")
    last_finish_reason: Option<inference_providers::FinishReason>,
    /// Last error from provider (for determining stop_reason)
    last_error: Option<inference_providers::CompletionError>,
    state: StreamState,
    /// Whether the model supports TEE attestation (false for external providers)
    attestation_supported: bool,
}

impl<S> InterceptStream<S>
where
    S: Stream<Item = Result<SSEEvent, inference_providers::CompletionError>> + Unpin,
{
    /// Store attestation signature before sending [DONE] to client.
    /// This runs in the hot path to ensure signature is available when client receives [DONE].
    /// Skipped for external providers that don't support TEE attestation.
    fn create_signature_future(&self) -> FinalizeFuture {
        // Skip attestation for external providers (OpenAI, Anthropic, Gemini, etc.)
        if !self.attestation_supported {
            return Box::pin(async {});
        }

        let chat_id = match &self.last_chat_id {
            Some(id) => id.clone(),
            None => {
                tracing::warn!("Cannot store signature: no chat_id received in stream");
                return Box::pin(async {});
            }
        };

        let attestation_service = self.attestation_service.clone();

        Box::pin(async move {
            match tokio::time::timeout(
                Duration::from_secs(FINALIZE_TIMEOUT_SECS),
                attestation_service.store_chat_signature_from_provider(&chat_id),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::error!("Failed to store chat signature: {:?}", e);
                }
                Err(_elapsed) => {
                    tracing::error!(
                        "Timeout storing chat signature after {}s",
                        FINALIZE_TIMEOUT_SECS
                    );
                }
            }
        })
    }

    /// Record usage and metrics. Called from Drop to ensure it always runs.
    fn record_usage_and_metrics(&self) {
        let organization_id = self.organization_id;
        let workspace_id = self.workspace_id;
        let api_key_id = self.api_key_id;
        let model_id = self.model_id;
        let inference_type = self.inference_type.clone();

        // Create span with context BEFORE any early returns so all error logs have context
        let _span = tracing::error_span!(
            "stream_drop",
            %organization_id,
            %workspace_id,
            %api_key_id,
            %model_id,
            %inference_type
        )
        .entered();

        let (input_tokens, output_tokens, chat_id) =
            match (&self.last_usage_stats, &self.last_chat_id) {
                (Some(usage), Some(chat_id)) => (
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    chat_id.clone(),
                ),
                (None, None) => {
                    tracing::error!("Stream ended but no usage stats and no chat_id available");
                    return;
                }
                (None, Some(chat_id)) => {
                    tracing::error!(%chat_id, "Stream ended but no usage stats available");
                    return;
                }
                (Some(usage), None) => {
                    tracing::error!(
                        prompt_tokens = usage.prompt_tokens,
                        completion_tokens = usage.completion_tokens,
                        "Stream ended but no chat_id available"
                    );
                    return;
                }
            };

        if input_tokens == 0 && output_tokens == 0 {
            return;
        }

        // Check if we're in a Tokio runtime context
        // Drop can be called outside of an async context (e.g., during shutdown)
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => {
                tracing::error!("Cannot record usage: no Tokio runtime available");
                return;
            }
        };

        let inference_id = hash_inference_id_to_uuid(&chat_id);

        // Create span with full context for async task
        let span = tracing::info_span!(
            "record_usage",
            %organization_id,
            %workspace_id,
            %api_key_id,
            %model_id,
            %inference_type,
            %inference_id
        );
        let last_finish_reason = self.last_finish_reason.clone();
        let last_error = self.last_error.clone();
        let response_id = self.response_id.clone();
        let usage_service = self.usage_service.clone();
        let metrics_service = self.metrics_service.clone();
        let ttft_ms = self.ttft_ms;
        let e2e_duration = self.service_start_time.elapsed();
        let first_token_time = self.first_token_time;
        let stream_completed = self.stream_completed;

        let avg_itl_ms = if self.token_count > 0 {
            Some(self.total_itl_ms / self.token_count as f64)
        } else {
            None
        };

        let input_bucket = get_input_bucket(input_tokens);
        let mut metric_tags = self.metric_tags.clone();
        metric_tags.push(format!("{TAG_INPUT_BUCKET}:{input_bucket}"));

        // Spawn critical billing operations on blocking thread pool with timeout
        // The tokio runtime waits for blocking tasks during graceful shutdown,
        // which helps prevent data loss compared to regular spawn
        let handle_clone = handle.clone();
        handle.spawn_blocking(move || {
            let _span_guard = span.enter();
            handle_clone.block_on(async move {
                let result = tokio::time::timeout(Duration::from_secs(2), async move {
                    let stop_reason = if let Some(ref err) = last_error {
                        Some(crate::usage::StopReason::from_completion_error(err))
                    } else if !stream_completed {
                        Some(crate::usage::StopReason::ClientDisconnect)
                    } else if let Some(ref finish_reason) = last_finish_reason {
                        Some(crate::usage::StopReason::from_provider_finish_reason(
                            finish_reason,
                        ))
                    } else {
                        Some(crate::usage::StopReason::Completed)
                    };

                    if usage_service
                        .record_usage(RecordUsageServiceRequest {
                            organization_id,
                            workspace_id,
                            api_key_id,
                            model_id,
                            input_tokens,
                            output_tokens,
                            inference_type,
                            ttft_ms,
                            avg_itl_ms,
                            inference_id: Some(inference_id),
                            provider_request_id: Some(chat_id),
                            stop_reason,
                            response_id,
                            image_count: None,
                        })
                        .await
                        .is_err()
                    {
                        tracing::error!("Failed to record usage");
                    }

                    // Record metrics
                    let tags: Vec<&str> = metric_tags.iter().map(|s| s.as_str()).collect();
                    metrics_service.record_latency(METRIC_LATENCY_TOTAL, e2e_duration, &tags);

                    if let Some(first_token_instant) = first_token_time {
                        let decoding_duration = first_token_instant.elapsed();
                        metrics_service.record_latency(
                            METRIC_LATENCY_DECODING_TIME,
                            decoding_duration,
                            &tags,
                        );

                        let decode_secs = decoding_duration.as_secs_f64();
                        if decode_secs > 0.0 {
                            let tps = output_tokens as f64 / decode_secs;
                            metrics_service.record_histogram(METRIC_TOKENS_PER_SECOND, tps, &tags);
                        }
                    }

                    metrics_service.record_count(METRIC_TOKENS_INPUT, input_tokens as i64, &tags);
                    metrics_service.record_count(METRIC_TOKENS_OUTPUT, output_tokens as i64, &tags);
                })
                .await;

                if result.is_err() {
                    tracing::error!(
                        "Timeout recording usage and metrics (2s exceeded), inference_id={}",
                        inference_id
                    );
                }
            })
        });
    }
}

impl<S> Stream for InterceptStream<S>
where
    S: Stream<Item = Result<SSEEvent, inference_providers::CompletionError>> + Unpin,
{
    type Item = Result<SSEEvent, inference_providers::CompletionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match &mut self.state {
                StreamState::Streaming => {
                    match Pin::new(&mut self.inner).poll_next(cx) {
                        Poll::Ready(Some(Ok(ref event))) => {
                            let now = Instant::now();

                            if !self.first_token_received {
                                self.first_token_received = true;
                                self.first_token_time = Some(now);
                                let backend_ttft = now.duration_since(self.provider_start_time);
                                let e2e_ttft = now.duration_since(self.service_start_time);
                                self.ttft_ms = Some(e2e_ttft.as_millis() as i32);
                                self.last_token_time = Some(now);
                                let tags_str: Vec<&str> =
                                    self.metric_tags.iter().map(|s| s.as_str()).collect();
                                self.metrics_service.record_latency(
                                    METRIC_LATENCY_TTFT,
                                    backend_ttft,
                                    &tags_str,
                                );
                                self.metrics_service.record_latency(
                                    METRIC_LATENCY_TTFT_TOTAL,
                                    e2e_ttft,
                                    &tags_str,
                                );
                            } else if let Some(last_time) = self.last_token_time {
                                // Calculate inter-token latency
                                let itl = now.duration_since(last_time);
                                self.total_itl_ms += itl.as_secs_f64() * 1000.0;
                                self.token_count += 1;
                                self.last_token_time = Some(now);
                            }

                            if let StreamChunk::Chat(ref chat_chunk) = event.chunk {
                                // Track chat_id for attestation (updated on each chunk)
                                self.last_chat_id = Some(chat_chunk.id.clone());

                                // Track usage stats (updated on each chunk that has usage)
                                if let Some(usage) = &chat_chunk.usage {
                                    self.last_usage_stats = Some(usage.clone());
                                }

                                // Track finish_reason from the final chunk (only set once at end)
                                if let Some(choice) = chat_chunk.choices.first() {
                                    if let Some(ref reason) = choice.finish_reason {
                                        self.last_finish_reason = Some(reason.clone());
                                    }
                                }
                            }
                            return Poll::Ready(Some(Ok(event.clone())));
                        }
                        Poll::Ready(None) => {
                            self.stream_completed = true;
                            let signature_future = self.create_signature_future();
                            self.state = StreamState::Finalizing(signature_future);
                        }
                        Poll::Ready(Some(Err(ref err))) => {
                            // Capture error for stop_reason in usage recording (handled in Drop)
                            // Note: We intentionally skip Finalizing state (attestation) for errors
                            // because partial completions cannot be verified by clients
                            self.last_error = Some(err.clone());
                            return Poll::Ready(Some(Err(err.clone())));
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                StreamState::Finalizing(ref mut future) => match future.as_mut().poll(cx) {
                    Poll::Ready(()) => {
                        self.state = StreamState::Done;
                        return Poll::Ready(None);
                    }
                    Poll::Pending => return Poll::Pending,
                },
                StreamState::Done => return Poll::Ready(None),
            }
        }
    }
}

impl<S> Drop for InterceptStream<S>
where
    S: Stream<Item = Result<SSEEvent, inference_providers::CompletionError>> + Unpin,
{
    fn drop(&mut self) {
        // Decrement concurrent counter if present
        if let Some(counter) = &self.concurrent_counter {
            counter.fetch_sub(1, Ordering::Release);
        }

        // Always record usage in Drop (async, fire-and-forget)
        self.record_usage_and_metrics();
    }
}

pub struct CompletionServiceImpl {
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub attestation_service: Arc<dyn AttestationServiceTrait>,
    pub usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
    pub metrics_service: Arc<dyn MetricsServiceTrait>,
    pub models_repository: Arc<dyn ModelsRepository>,
    concurrent_counts: Cache<(Uuid, Uuid), Arc<AtomicU32>>,
    concurrent_limit: u32,
    /// Cache for per-organization concurrent limits (5-minute TTL)
    org_concurrent_limits: Cache<Uuid, u32>,
    /// Repository for fetching organization concurrent limits
    organization_limit_repository: Arc<dyn ports::OrganizationConcurrentLimitRepository>,
    /// Inference timeout in seconds for service-level timeout protection
    inference_timeout_secs: u64,
}

/// TTL for organization concurrent limit cache (5 minutes)
const ORG_LIMIT_CACHE_TTL_SECS: u64 = 300;

impl CompletionServiceImpl {
    pub fn new(
        inference_provider_pool: Arc<InferenceProviderPool>,
        attestation_service: Arc<dyn AttestationServiceTrait>,
        usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
        metrics_service: Arc<dyn MetricsServiceTrait>,
        models_repository: Arc<dyn ModelsRepository>,
        organization_limit_repository: Arc<dyn ports::OrganizationConcurrentLimitRepository>,
    ) -> Self {
        Self::with_timeout(
            inference_provider_pool,
            attestation_service,
            usage_service,
            metrics_service,
            models_repository,
            organization_limit_repository,
            300, // Default 5 minutes
        )
    }

    pub fn with_timeout(
        inference_provider_pool: Arc<InferenceProviderPool>,
        attestation_service: Arc<dyn AttestationServiceTrait>,
        usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
        metrics_service: Arc<dyn MetricsServiceTrait>,
        models_repository: Arc<dyn ModelsRepository>,
        organization_limit_repository: Arc<dyn ports::OrganizationConcurrentLimitRepository>,
        inference_timeout_secs: u64,
    ) -> Self {
        let concurrent_counts = Cache::builder().max_capacity(100_000).build();

        // Cache for per-organization concurrent limits with 5-minute TTL
        let org_concurrent_limits = Cache::builder()
            .time_to_live(Duration::from_secs(ORG_LIMIT_CACHE_TTL_SECS))
            .max_capacity(10_000)
            .build();

        Self {
            inference_provider_pool,
            attestation_service,
            usage_service,
            metrics_service,
            models_repository,
            concurrent_counts,
            concurrent_limit: DEFAULT_CONCURRENT_LIMIT,
            org_concurrent_limits,
            organization_limit_repository,
            inference_timeout_secs,
        }
    }

    /// Get the concurrent request limit for an organization (cached)
    async fn get_org_concurrent_limit(&self, organization_id: Uuid) -> u32 {
        let default_limit = self.concurrent_limit;
        let repo = self.organization_limit_repository.clone();

        self.org_concurrent_limits
            .get_with(organization_id, async move {
                match repo.get_concurrent_limit(organization_id).await {
                    Ok(Some(limit)) if limit > 0 => limit,
                    Ok(_) => default_limit, // Use default if NULL or 0
                    Err(e) => {
                        tracing::warn!(
                            organization_id = %organization_id,
                            error = %e,
                            "Failed to fetch org concurrent limit, using default"
                        );
                        default_limit
                    }
                }
            })
            .await
    }

    /// Create low-cardinality metric tags for a request
    ///
    /// These tags are used for OTLP/Datadog metrics and should only include
    /// low-cardinality values to minimize costs (~98% savings vs high-cardinality).
    /// High-cardinality data (org/workspace/key) is tracked via database analytics.
    fn create_metric_tags(model_name: &str) -> Vec<String> {
        let environment = get_environment();
        vec![
            format!("{}:{}", TAG_MODEL, model_name),
            format!("{}:{}", TAG_ENVIRONMENT, environment),
        ]
    }

    fn map_provider_error(
        model: &str,
        error: &inference_providers::CompletionError,
        operation: &str,
    ) -> ports::CompletionError {
        match error {
            inference_providers::CompletionError::HttpError { status_code, .. } => match *status_code
            {
                503 => ports::CompletionError::ServiceOverloaded(
                    "The service is temporarily overloaded. Please retry with exponential backoff."
                        .to_string(),
                ),
                400..=499 => {
                    tracing::warn!(model, status_code, "Client error during {}", operation);
                    ports::CompletionError::InvalidParams(
                        "Invalid request parameters. Please check your input and try again."
                            .to_string(),
                    )
                }
                _ => {
                    tracing::error!(model, status_code, "Provider error during {}", operation);
                    ports::CompletionError::ProviderError(
                        "The model is currently unavailable. Please try again later.".to_string(),
                    )
                }
            },
            _ => {
                tracing::error!(model, "Provider error during {}: {}", operation, error);
                ports::CompletionError::ProviderError(
                    "The model is currently unavailable. Please try again later.".to_string(),
                )
            }
        }
    }

    /// Record an error metric with the appropriate error type tag
    fn record_error(&self, error: &ports::CompletionError, model_name: Option<&str>) {
        let error_type = match error {
            ports::CompletionError::InvalidModel(_) => ERROR_TYPE_INVALID_MODEL,
            ports::CompletionError::InvalidParams(_) => ERROR_TYPE_INVALID_PARAMS,
            ports::CompletionError::RateLimitExceeded => ERROR_TYPE_RATE_LIMIT,
            ports::CompletionError::ProviderError(_) => ERROR_TYPE_INFERENCE_ERROR,
            ports::CompletionError::ServiceOverloaded(_) => ERROR_TYPE_SERVICE_OVERLOADED,
            ports::CompletionError::InternalError(_) => ERROR_TYPE_INTERNAL_ERROR,
        };

        let environment = get_environment();
        let mut tags = vec![
            format!("{}:{}", TAG_ERROR_TYPE, error_type),
            format!("{}:{}", TAG_ENVIRONMENT, environment),
        ];

        // Add model tag if available (for model-specific errors)
        if let Some(model) = model_name {
            tags.push(format!("{TAG_MODEL}:{model}"));
        }

        let tags_str: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
        self.metrics_service
            .record_count(METRIC_REQUEST_ERRORS, 1, &tags_str);
    }

    /// Convert completion messages to chat messages for inference providers
    fn prepare_chat_messages(messages: &[ports::CompletionMessage]) -> Vec<ChatMessage> {
        messages
            .iter()
            .map(|msg| ChatMessage {
                role: match msg.role.as_str() {
                    "system" => MessageRole::System,
                    "assistant" => MessageRole::Assistant,
                    "tool" => MessageRole::Tool,
                    _ => MessageRole::User,
                },
                content: Some(serde_json::Value::String(msg.content.clone())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            })
            .collect()
    }

    async fn try_acquire_concurrent_slot(
        &self,
        organization_id: Uuid,
        model_id: Uuid,
        model_name: &str,
    ) -> Result<Arc<AtomicU32>, ports::CompletionError> {
        // Get the dynamic limit for this organization (cached with 5-min TTL)
        let limit = self.get_org_concurrent_limit(organization_id).await;

        let counter = self
            .concurrent_counts
            .get_with((organization_id, model_id), async {
                Arc::new(AtomicU32::new(0))
            })
            .await;

        loop {
            let current = counter.load(Ordering::Acquire);
            if current >= limit {
                tracing::warn!(
                    organization_id = %organization_id,
                    model_id = %model_id,
                    model_name = %model_name,
                    current_count = current,
                    limit = limit,
                    "Organization concurrent request limit exceeded for model"
                );
                self.record_error(&ports::CompletionError::RateLimitExceeded, Some(model_name));
                return Err(ports::CompletionError::RateLimitExceeded);
            }
            if counter
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(counter);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_stream_with_context(
        &self,
        llm_stream: StreamingResult,
        organization_id: Uuid,
        workspace_id: Uuid,
        api_key_id: Uuid,
        model_id: Uuid,
        model_name: String,
        inference_type: &str,
        service_start_time: Instant,
        provider_start_time: Instant,
        concurrent_counter: Option<Arc<AtomicU32>>,
        response_id: Option<ResponseId>,
        attestation_supported: bool,
    ) -> StreamingResult {
        // Create low-cardinality metric tags (no org/workspace/key - those go to database)
        let metric_tags = Self::create_metric_tags(&model_name);

        let tags_str: Vec<&str> = metric_tags.iter().map(|s| s.as_str()).collect();
        self.metrics_service
            .record_count(METRIC_REQUEST_COUNT, 1, &tags_str);

        let queue_time = provider_start_time.duration_since(service_start_time);
        self.metrics_service
            .record_latency(METRIC_LATENCY_QUEUE_TIME, queue_time, &tags_str);

        let intercepted_stream = InterceptStream {
            inner: llm_stream,
            attestation_service: self.attestation_service.clone(),
            usage_service: self.usage_service.clone(),
            metrics_service: self.metrics_service.clone(),
            organization_id,
            workspace_id,
            api_key_id,
            model_id,
            model_name,
            inference_type: inference_type.to_string(),
            service_start_time,
            provider_start_time,
            first_token_received: false,
            first_token_time: None,
            ttft_ms: None,
            token_count: 0,
            last_token_time: None,
            total_itl_ms: 0.0,
            metric_tags,
            concurrent_counter,
            last_usage_stats: None,
            last_chat_id: None,
            stream_completed: false,
            response_id,
            last_finish_reason: None,
            last_error: None,
            state: StreamState::Streaming,
            attestation_supported,
        };
        Box::pin(intercepted_stream)
    }
}

#[async_trait::async_trait]
impl ports::CompletionServiceTrait for CompletionServiceImpl {
    async fn create_chat_completion_stream(
        &self,
        request: ports::CompletionRequest,
    ) -> Result<StreamingResult, ports::CompletionError> {
        let service_start_time = Instant::now();

        // Extract context for usage tracking
        let organization_id = request.organization_id;
        let workspace_id = request.workspace_id;
        let api_key_id = match uuid::Uuid::parse_str(&request.api_key_id) {
            Ok(id) => id,
            Err(e) => {
                let err = ports::CompletionError::InvalidParams(format!("Invalid API key ID: {e}"));
                self.record_error(&err, None);
                return Err(err);
            }
        };
        let is_streaming = request.stream.unwrap_or(false);

        let chat_messages = Self::prepare_chat_messages(&request.messages);

        let mut chat_params = inference_providers::ChatCompletionParams {
            model: request.model.clone(),
            messages: chat_messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            top_p: request.top_p,
            stop: request.stop,
            stream: Some(true),
            tools: None,
            max_completion_tokens: None,
            n: request.n,
            frequency_penalty: None,
            presence_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: Some(request.user_id.to_string()),
            seed: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: request.metadata,
            store: None,
            stream_options: None,
            modalities: None,
            extra: request.extra.clone(),
        };

        // Resolve model name (could be an alias) and get model details in a single DB call
        // This also validates that the model exists and is active
        let model = match self
            .models_repository
            .resolve_and_get_model(&request.model)
            .await
        {
            Ok(Some(m)) => m,
            Ok(None) => {
                let err = ports::CompletionError::InvalidModel(format!(
                    "Model '{}' not found. It's not a valid model name or alias.",
                    request.model
                ));
                // Do not record the invalid model name in metrics to avoid high cardinality
                self.record_error(&err, None);
                return Err(err);
            }
            Err(e) => {
                let err =
                    ports::CompletionError::InternalError(format!("Failed to resolve model: {e}"));
                // Do not record the possibly invalid model name in metrics
                self.record_error(&err, None);
                return Err(err);
            }
        };

        let canonical_name = &model.model_name;

        // Update params with canonical name if it's different
        if canonical_name != &request.model {
            tracing::debug!(
                requested_model = %request.model,
                canonical_model = %canonical_name,
                "Resolved alias to canonical model name"
            );
            chat_params.model = canonical_name.clone();
        }

        let counter = self
            .try_acquire_concurrent_slot(organization_id, model.id, canonical_name)
            .await?;

        let provider_start_time = Instant::now();

        // Get the LLM stream
        let llm_stream = match self
            .inference_provider_pool
            .chat_completion_stream(chat_params, request.body_hash.clone())
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                counter.fetch_sub(1, Ordering::Release);
                let err = Self::map_provider_error(&request.model, &e, "chat completion stream");
                self.record_error(&err, Some(canonical_name));
                return Err(err);
            }
        };

        let inference_type = if is_streaming {
            "chat_completion_stream"
        } else {
            "chat_completion"
        };

        // Create the completion event stream with usage tracking
        // Use model UUID for usage tracking, model name for low-cardinality metrics
        let event_stream = self
            .handle_stream_with_context(
                llm_stream,
                organization_id,
                workspace_id,
                api_key_id,
                model.id,
                model.model_name.clone(),
                inference_type,
                service_start_time,
                provider_start_time,
                Some(counter),
                request.response_id,
                model.attestation_supported,
            )
            .await;

        Ok(event_stream)
    }

    async fn create_chat_completion(
        &self,
        request: ports::CompletionRequest,
    ) -> Result<inference_providers::ChatCompletionResponseWithBytes, ports::CompletionError> {
        let service_start_time = Instant::now();
        let chat_messages = Self::prepare_chat_messages(&request.messages);

        let mut chat_params = inference_providers::ChatCompletionParams {
            model: request.model.clone(),
            messages: chat_messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            top_p: request.top_p,
            stop: request.stop,
            stream: Some(false),
            tools: None,
            max_completion_tokens: None,
            n: request.n,
            frequency_penalty: None,
            presence_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: Some(request.user_id.to_string()),
            seed: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: request.metadata,
            store: None,
            stream_options: None,
            modalities: None,
            extra: request.extra.clone(),
        };

        // Resolve model name (could be an alias) and get model details in a single DB call
        // This also validates that the model exists and is active
        let model = match self
            .models_repository
            .resolve_and_get_model(&request.model)
            .await
        {
            Ok(Some(m)) => m,
            Ok(None) => {
                let err = ports::CompletionError::InvalidModel(format!(
                    "Model '{}' not found. It's not a valid model name or alias.",
                    request.model
                ));
                // Do not record the invalid model name in metrics to avoid high cardinality
                self.record_error(&err, None);
                return Err(err);
            }
            Err(e) => {
                let err =
                    ports::CompletionError::InternalError(format!("Failed to resolve model: {e}"));
                // Do not record the possibly invalid model name in metrics
                self.record_error(&err, None);
                return Err(err);
            }
        };

        let canonical_name = &model.model_name;

        let api_key_id = match uuid::Uuid::parse_str(&request.api_key_id) {
            Ok(id) => id,
            Err(e) => {
                let err = ports::CompletionError::InvalidParams(format!("Invalid API key ID: {e}"));
                self.record_error(&err, Some(canonical_name));
                return Err(err);
            }
        };

        // Update params with canonical name if it's different
        if canonical_name != &request.model {
            tracing::debug!(
                requested_model = %request.model,
                canonical_model = %canonical_name,
                "Resolved alias to canonical model name"
            );
            chat_params.model = canonical_name.clone();
        }

        let organization_id = request.organization_id;
        let counter = self
            .try_acquire_concurrent_slot(organization_id, model.id, canonical_name)
            .await?;

        let provider_start_time = Instant::now();
        let result = self
            .inference_provider_pool
            .chat_completion(chat_params, request.body_hash.clone())
            .await;
        counter.fetch_sub(1, Ordering::Release);

        let response_with_bytes = match result {
            Ok(response) => response,
            Err(e) => {
                let err = Self::map_provider_error(&request.model, &e, "chat completion");
                self.record_error(&err, Some(canonical_name));
                return Err(err);
            }
        };

        let e2e_latency = service_start_time.elapsed();
        let backend_latency = provider_start_time.elapsed();
        let queue_time = provider_start_time.duration_since(service_start_time);

        // Store attestation signature (only for models that support TEE attestation)
        if model.attestation_supported {
            let attestation_service = self.attestation_service.clone();
            let chat_id = response_with_bytes.response.id.clone();
            tokio::spawn(async move {
                if attestation_service
                    .store_chat_signature_from_provider(chat_id.as_str())
                    .await
                    .is_err()
                {
                    tracing::error!("Failed to store chat signature");
                } else {
                    tracing::debug!("Stored signature for chat_id: {}", chat_id);
                }
            });
        }

        // Record metrics with low-cardinality tags only
        let metrics_service = self.metrics_service.clone();
        let input_tokens = response_with_bytes.response.usage.prompt_tokens;
        let output_tokens = response_with_bytes.response.usage.completion_tokens;
        let model_name = model.model_name.clone();

        tokio::spawn(async move {
            let mut tags = CompletionServiceImpl::create_metric_tags(&model_name);
            let input_bucket = get_input_bucket(input_tokens);
            tags.push(format!("{TAG_INPUT_BUCKET}:{input_bucket}"));
            let tags_str: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();

            metrics_service.record_count(METRIC_REQUEST_COUNT, 1, &tags_str);
            metrics_service.record_latency(METRIC_LATENCY_QUEUE_TIME, queue_time, &tags_str);
            metrics_service.record_latency(METRIC_LATENCY_TTFT, backend_latency, &tags_str);
            metrics_service.record_latency(METRIC_LATENCY_TTFT_TOTAL, e2e_latency, &tags_str);
            metrics_service.record_latency(METRIC_LATENCY_TOTAL, e2e_latency, &tags_str);

            if backend_latency.as_secs_f64() > 0.0 {
                let tps = output_tokens as f64 / backend_latency.as_secs_f64();
                metrics_service.record_histogram(METRIC_TOKENS_PER_SECOND, tps, &tags_str);
            }

            metrics_service.record_count(METRIC_TOKENS_INPUT, input_tokens as i64, &tags_str);
            metrics_service.record_count(METRIC_TOKENS_OUTPUT, output_tokens as i64, &tags_str);
        });

        // Record usage with model UUID
        // Note: TTFT doesn't apply to non-streaming (you get all tokens at once)
        let usage_service = self.usage_service.clone();
        let workspace_id = request.workspace_id;
        let model_id = model.id;
        let input_tokens = response_with_bytes.response.usage.prompt_tokens;
        let output_tokens = response_with_bytes.response.usage.completion_tokens;
        // Hash the full chat ID to UUID for storage
        let provider_request_id = response_with_bytes.response.id.clone();
        let inference_id = hash_inference_id_to_uuid(&provider_request_id);
        let response_id = request.response_id;

        // Extract finish_reason from provider response
        let stop_reason = response_with_bytes
            .response
            .choices
            .first()
            .and_then(|c| c.finish_reason.as_ref())
            .map(|reason| crate::usage::StopReason::from_finish_reason(reason))
            .unwrap_or(crate::usage::StopReason::Completed);

        tokio::spawn(async move {
            if usage_service
                .record_usage(RecordUsageServiceRequest {
                    organization_id,
                    workspace_id,
                    api_key_id,
                    model_id,
                    input_tokens,
                    output_tokens,
                    inference_type: "chat_completion".to_string(),
                    ttft_ms: None,    // N/A for non-streaming
                    avg_itl_ms: None, // N/A for non-streaming
                    inference_id: Some(inference_id),
                    provider_request_id: Some(provider_request_id),
                    stop_reason: Some(stop_reason),
                    response_id,
                    image_count: None,
                })
                .await
                .is_err()
            {
                tracing::error!("Failed to record usage in completion service");
            } else {
                tracing::debug!(
                    "Recorded usage for org {}: {} input, {} output tokens (api_key: {})",
                    organization_id,
                    input_tokens,
                    output_tokens,
                    api_key_id
                );
            }
        });

        Ok(response_with_bytes)
    }

    #[allow(clippy::too_many_arguments)]
    async fn try_score(
        &self,
        organization_id: uuid::Uuid,
        workspace_id: uuid::Uuid,
        model_id: uuid::Uuid,
        model_name: &str,
        api_key_id: Uuid,
        params: inference_providers::ScoreParams,
        request_hash: String,
    ) -> Result<inference_providers::ScoreResponse, ports::CompletionError> {
        // Acquire concurrent request slot to enforce organization limits
        let counter = self
            .try_acquire_concurrent_slot(organization_id, model_id, model_name)
            .await?;

        // RAII guard to ensure slot is always released, even on panic or task cancellation
        struct SlotGuard {
            counter: Arc<AtomicU32>,
        }
        impl Drop for SlotGuard {
            fn drop(&mut self) {
                self.counter.fetch_sub(1, Ordering::Release);
            }
        }
        let _guard = SlotGuard {
            counter: counter.clone(),
        };

        // Call inference provider pool with service-level timeout protection
        // Defense-in-depth: even if backend timeout fails, we still protect the concurrent slot
        let response = match tokio::time::timeout(
            Duration::from_secs(self.inference_timeout_secs),
            self.inference_provider_pool.score(params, request_hash),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                // Provider error - map ScoreError to CompletionError
                let error_msg = match e {
                    ScoreError::GenerationError(msg) => msg,
                    ScoreError::HttpError {
                        status_code,
                        message,
                    } => {
                        format!("HTTP {}: {}", status_code, message)
                    }
                };
                return Err(ports::CompletionError::ProviderError(error_msg));
            }
            Err(_) => {
                // Timeout error
                tracing::warn!(
                    "Score request timeout after {} seconds",
                    self.inference_timeout_secs
                );
                return Err(ports::CompletionError::ProviderError(format!(
                    "Scoring request timed out after {} seconds",
                    self.inference_timeout_secs
                )));
            }
        };

        // CRITICAL: Record usage BEFORE releasing the concurrent slot
        // This prevents a race condition where:
        // 1. Provider call succeeds
        // 2. Slot is released (guard dropped)
        // 3. Another request acquires the slot
        // 4. Usage recording fails → returns 500 without billing
        // 5. Organization bypasses concurrent limits without being charged
        //
        // By recording usage here (while holding the slot), we ensure
        // atomicity: either the full request (inference + billing) succeeds,
        // or the slot is released after an error is returned.
        let token_count = response
            .usage
            .as_ref()
            .and_then(|u| u.prompt_tokens)
            .unwrap_or(0);

        let inference_id = uuid::Uuid::new_v4();
        let usage_request = RecordUsageServiceRequest {
            organization_id,
            workspace_id,
            api_key_id,
            model_id,
            input_tokens: token_count,
            output_tokens: 0,
            inference_type: "score".to_string(),
            ttft_ms: None,
            avg_itl_ms: None,
            inference_id: Some(inference_id),
            provider_request_id: None,
            stop_reason: Some(crate::usage::StopReason::Completed),
            response_id: None,
            image_count: None,
        };

        // Record usage synchronously - this is billing-critical and must succeed
        // If this fails, we return an error while still holding the slot
        // This ensures the organization is charged OR the request fails atomically
        if let Err(e) = self.usage_service.record_usage(usage_request).await {
            tracing::error!(error = %e, "Failed to record score usage - request will fail");
            return Err(ports::CompletionError::InternalError(
                "Failed to record usage - please retry".to_string(),
            ));
        }

        // Slot is released here via Drop (after usage is recorded)
        Ok(response)
    }
}

pub use ports::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::capturing::{CapturingMetricsService, MetricValue};
    use crate::test_utils::{MockAttestationService, MockUsageService};
    use bytes::Bytes;
    use futures::{stream, StreamExt};
    use inference_providers::models::{ChatChoice, ChatCompletionChunk, FinishReason, TokenUsage};
    use std::time::Duration;

    #[tokio::test]
    async fn test_intercept_stream_metrics() {
        let metrics_service = Arc::new(CapturingMetricsService::new());
        let attestation_service = Arc::new(MockAttestationService);
        let usage_service = Arc::new(MockUsageService);

        let organization_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let api_key_id = Uuid::new_v4();
        let model_id = Uuid::new_v4();

        // Create a stream with a content chunk and a usage chunk
        let content_chunk = SSEEvent {
            raw_bytes: Bytes::from("data: ..."),
            chunk: StreamChunk::Chat(ChatCompletionChunk {
                id: "chat-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: "test-model".to_string(),
                choices: vec![],
                usage: None,
                prompt_token_ids: None,
                system_fingerprint: None,
                modality: None,
            }),
        };

        let usage_chunk = SSEEvent {
            raw_bytes: Bytes::from("data: ..."),
            chunk: StreamChunk::Chat(ChatCompletionChunk {
                id: "chat-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: "test-model".to_string(),
                choices: vec![ChatChoice {
                    index: 0,
                    delta: None,
                    logprobs: None,
                    finish_reason: Some(FinishReason::Stop),
                    token_ids: None,
                }],
                usage: Some(TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    total_tokens: 30,
                    prompt_tokens_details: None,
                }),
                prompt_token_ids: None,
                system_fingerprint: None,
                modality: None,
            }),
        };

        let stream = stream::iter(vec![Ok(content_chunk), Ok(usage_chunk)]);

        let metric_tags = CompletionServiceImpl::create_metric_tags("test-model");

        let now = Instant::now();
        let intercept_stream = InterceptStream {
            inner: stream,
            attestation_service,
            usage_service,
            metrics_service: metrics_service.clone(),
            organization_id,
            workspace_id,
            api_key_id,
            model_id,
            model_name: "test-model".to_string(),
            inference_type: "chat_completion_stream".to_string(),
            service_start_time: now,
            provider_start_time: now,
            first_token_received: false,
            first_token_time: None,
            ttft_ms: None,
            token_count: 0,
            last_token_time: None,
            total_itl_ms: 0.0,
            metric_tags,
            concurrent_counter: None,
            last_usage_stats: None,
            last_chat_id: None,
            stream_completed: false,
            response_id: None,
            last_finish_reason: None,
            last_error: None,
            state: StreamState::Streaming,
            attestation_supported: true,
        };

        // Consume the stream
        let _ = intercept_stream.collect::<Vec<_>>().await;

        // Wait for async usage recording in Drop to complete
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify metrics
        let metrics = metrics_service.get_metrics();

        // Should have:
        // 1. latency.time_to_first_token (Backend TTFT from first chunk)
        // 2. latency.time_to_first_token_total (E2E TTFT from first chunk)
        // 3. latency.total (from Drop handler)
        // 4. tokens_per_second (from Drop handler)
        assert!(
            metrics.len() >= 4,
            "Expected at least 4 metrics, got {}",
            metrics.len()
        );

        let ttft = metrics
            .iter()
            .find(|m| m.name == METRIC_LATENCY_TTFT)
            .expect("TTFT metric missing");
        assert!(matches!(ttft.value, MetricValue::Latency(_)));
        assert!(ttft
            .tags
            .contains(&format!("{}:{}", TAG_MODEL, "test-model")));

        let total_latency = metrics
            .iter()
            .find(|m| m.name == METRIC_LATENCY_TOTAL)
            .expect("Total latency metric missing");
        assert!(matches!(total_latency.value, MetricValue::Latency(_)));

        let tps = metrics
            .iter()
            .find(|m| m.name == METRIC_TOKENS_PER_SECOND)
            .expect("TPS metric missing");
        if let MetricValue::Histogram(val) = tps.value {
            assert!(val > 0.0);
        } else {
            panic!("TPS should be a histogram");
        }
    }

    #[tokio::test]
    async fn test_intercept_stream_captures_ttft_and_itl() {
        use crate::test_utils::CapturingUsageService;

        let metrics_service = Arc::new(CapturingMetricsService::new());
        let attestation_service = Arc::new(MockAttestationService);
        let usage_service = Arc::new(CapturingUsageService::new());

        let organization_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let api_key_id = Uuid::new_v4();
        let model_id = Uuid::new_v4();

        // Create multiple content chunks to test ITL calculation
        let chunk1 = SSEEvent {
            raw_bytes: Bytes::from("data: chunk1"),
            chunk: StreamChunk::Chat(ChatCompletionChunk {
                id: "chat-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: "test-model".to_string(),
                choices: vec![],
                usage: None,
                prompt_token_ids: None,
                system_fingerprint: None,
                modality: None,
            }),
        };

        let chunk2 = SSEEvent {
            raw_bytes: Bytes::from("data: chunk2"),
            chunk: StreamChunk::Chat(ChatCompletionChunk {
                id: "chat-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: "test-model".to_string(),
                choices: vec![],
                usage: None,
                prompt_token_ids: None,
                system_fingerprint: None,
                modality: None,
            }),
        };

        let usage_chunk = SSEEvent {
            raw_bytes: Bytes::from("data: usage"),
            chunk: StreamChunk::Chat(ChatCompletionChunk {
                id: "chat-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: "test-model".to_string(),
                choices: vec![ChatChoice {
                    index: 0,
                    delta: None,
                    logprobs: None,
                    finish_reason: Some(FinishReason::Stop),
                    token_ids: None,
                }],
                usage: Some(TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    total_tokens: 30,
                    prompt_tokens_details: None,
                }),
                prompt_token_ids: None,
                modality: None,
                system_fingerprint: None,
            }),
        };

        // Simulate a stream with delays between chunks
        let stream = stream::iter(vec![Ok(chunk1), Ok(chunk2), Ok(usage_chunk)]);

        let metric_tags = CompletionServiceImpl::create_metric_tags("test-model");

        // Use a start time from "before" to simulate real TTFT
        let service_start_time = Instant::now() - Duration::from_millis(50);
        let provider_start_time = Instant::now() - Duration::from_millis(25);

        let intercept_stream = InterceptStream {
            inner: stream,
            attestation_service,
            usage_service: usage_service.clone(),
            metrics_service: metrics_service.clone(),
            organization_id,
            workspace_id,
            api_key_id,
            model_id,
            model_name: "test-model".to_string(),
            inference_type: "chat_completion_stream".to_string(),
            service_start_time,
            provider_start_time,
            first_token_received: false,
            first_token_time: None,
            ttft_ms: None,
            token_count: 0,
            last_token_time: None,
            total_itl_ms: 0.0,
            metric_tags,
            concurrent_counter: None,
            last_usage_stats: None,
            last_chat_id: None,
            stream_completed: false,
            response_id: None,
            last_finish_reason: None,
            last_error: None,
            state: StreamState::Streaming,
            attestation_supported: true,
        };

        // Consume the stream
        let _ = intercept_stream.collect::<Vec<_>>().await;

        // Wait for async usage recording in Drop to complete
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify usage was recorded with latency metrics
        let requests = usage_service.get_requests();
        assert_eq!(requests.len(), 1, "Expected exactly one usage request");

        let req = &requests[0];
        assert_eq!(req.input_tokens, 10);
        assert_eq!(req.output_tokens, 20);

        // TTFT should be captured (>= 50ms since we set start_time 50ms in the past)
        assert!(
            req.ttft_ms.is_some(),
            "TTFT should be captured for streaming"
        );
        assert!(
            req.ttft_ms.unwrap() >= 50,
            "TTFT should be at least 50ms, got {:?}",
            req.ttft_ms
        );

        // ITL should be captured (we had 2 chunks after first token)
        assert!(
            req.avg_itl_ms.is_some(),
            "avg_itl_ms should be captured for streaming with multiple chunks"
        );
    }

    #[tokio::test]
    async fn test_create_metric_tags_includes_model_and_environment() {
        let tags = CompletionServiceImpl::create_metric_tags("gpt-4");

        assert_eq!(tags.len(), 2);
        assert!(tags.iter().any(|t| t.starts_with("model:")));
        assert!(tags.iter().any(|t| t.starts_with("environment:")));
        assert!(tags.iter().any(|t| t == "model:gpt-4"));
    }

    #[tokio::test]
    async fn test_intercept_stream_single_chunk_no_itl() {
        use crate::test_utils::CapturingUsageService;

        let metrics_service = Arc::new(CapturingMetricsService::new());
        let attestation_service = Arc::new(MockAttestationService);
        let usage_service = Arc::new(CapturingUsageService::new());

        let organization_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let api_key_id = Uuid::new_v4();
        let model_id = Uuid::new_v4();

        // Single chunk with usage (no inter-token latency to measure)
        let usage_chunk = SSEEvent {
            raw_bytes: Bytes::from("data: usage"),
            chunk: StreamChunk::Chat(ChatCompletionChunk {
                id: "chat-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: "test-model".to_string(),
                choices: vec![ChatChoice {
                    index: 0,
                    delta: None,
                    logprobs: None,
                    finish_reason: Some(FinishReason::Stop),
                    token_ids: None,
                }],
                usage: Some(TokenUsage {
                    prompt_tokens: 5,
                    completion_tokens: 1,
                    total_tokens: 6,
                    prompt_tokens_details: None,
                }),
                prompt_token_ids: None,
                modality: None,
                system_fingerprint: None,
            }),
        };

        let stream = stream::iter(vec![Ok(usage_chunk)]);
        let metric_tags = CompletionServiceImpl::create_metric_tags("test-model");

        let now = Instant::now();
        let intercept_stream = InterceptStream {
            inner: stream,
            attestation_service,
            usage_service: usage_service.clone(),
            metrics_service: metrics_service.clone(),
            organization_id,
            workspace_id,
            api_key_id,
            model_id,
            model_name: "test-model".to_string(),
            inference_type: "chat_completion_stream".to_string(),
            service_start_time: now,
            provider_start_time: now,
            first_token_received: false,
            first_token_time: None,
            ttft_ms: None,
            token_count: 0,
            last_token_time: None,
            total_itl_ms: 0.0,
            metric_tags,
            concurrent_counter: None,
            last_usage_stats: None,
            last_chat_id: None,
            stream_completed: false,
            response_id: None,
            last_finish_reason: None,
            last_error: None,
            state: StreamState::Streaming,
            attestation_supported: true,
        };

        let _ = intercept_stream.collect::<Vec<_>>().await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let requests = usage_service.get_requests();
        assert_eq!(requests.len(), 1);

        let req = &requests[0];
        // TTFT should still be captured
        assert!(req.ttft_ms.is_some(), "TTFT should be captured");
        // ITL should be None since there's only one chunk (no inter-token gaps)
        assert!(
            req.avg_itl_ms.is_none(),
            "avg_itl_ms should be None for single chunk, got {:?}",
            req.avg_itl_ms
        );
    }

    #[tokio::test]
    async fn test_concurrent_limit_state() {
        let cache: Cache<(Uuid, Uuid), Arc<AtomicU32>> =
            Cache::builder().max_capacity(1000).build();

        let org_id = Uuid::new_v4();
        let model_id = Uuid::new_v4();
        let key = (org_id, model_id);
        let limit: u32 = 3;

        let mut counters = Vec::new();
        for i in 0..3 {
            let counter = cache
                .get_with(key, async { Arc::new(AtomicU32::new(0)) })
                .await;
            loop {
                let current = counter.load(Ordering::Acquire);
                assert!(
                    current < limit,
                    "Request {} should be under limit, got count {}",
                    i,
                    current
                );
                if counter
                    .compare_exchange_weak(
                        current,
                        current + 1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    break;
                }
            }
            counters.push(counter);
        }

        // 4th request should be over limit
        let counter = cache
            .get_with(key, async { Arc::new(AtomicU32::new(0)) })
            .await;
        let current = counter.load(Ordering::Acquire);
        assert!(
            current >= limit,
            "4th request should be over limit, got count {}",
            current
        );

        // Release one slot
        counters[0].fetch_sub(1, Ordering::Release);

        // Now another request should succeed
        let counter = cache
            .get_with(key, async { Arc::new(AtomicU32::new(0)) })
            .await;
        let current = counter.load(Ordering::Acquire);
        assert!(
            current < limit,
            "Request after release should succeed, got count {}",
            current
        );
    }

    #[tokio::test]
    async fn test_concurrent_limit_different_orgs_and_models_independent() {
        let cache: Cache<(Uuid, Uuid), Arc<AtomicU32>> =
            Cache::builder().max_capacity(1000).build();

        let org1 = Uuid::new_v4();
        let org2 = Uuid::new_v4();
        let model_a = Uuid::new_v4();
        let model_b = Uuid::new_v4();

        // Fill up org1 + model_a's limit
        let counter1 = cache
            .get_with((org1, model_a), async { Arc::new(AtomicU32::new(0)) })
            .await;
        counter1.fetch_add(64, Ordering::AcqRel);

        // org1 + model_b should still be able to make requests (different model)
        let counter2 = cache
            .get_with((org1, model_b), async { Arc::new(AtomicU32::new(0)) })
            .await;
        let current = counter2.load(Ordering::Acquire);
        assert_eq!(
            current, 0,
            "org1+model_b should start at 0, got {}",
            current
        );

        // org2 + model_a should still be able to make requests (different org)
        let counter3 = cache
            .get_with((org2, model_a), async { Arc::new(AtomicU32::new(0)) })
            .await;
        let current = counter3.load(Ordering::Acquire);
        assert_eq!(
            current, 0,
            "org2+model_a should start at 0, got {}",
            current
        );
    }

    #[tokio::test]
    async fn test_intercept_stream_decrements_on_drop() {
        // Test that InterceptStream decrements the counter when dropped
        let counter = Arc::new(AtomicU32::new(1)); // Start at 1 (simulating acquired slot)

        {
            let metrics_service = Arc::new(CapturingMetricsService::new());
            let attestation_service = Arc::new(MockAttestationService);
            let usage_service = Arc::new(MockUsageService);

            let stream =
                stream::iter::<Vec<Result<SSEEvent, inference_providers::CompletionError>>>(vec![]);

            let _intercept_stream = InterceptStream {
                inner: stream,
                attestation_service,
                usage_service,
                metrics_service,
                organization_id: Uuid::new_v4(),
                workspace_id: Uuid::new_v4(),
                api_key_id: Uuid::new_v4(),
                model_id: Uuid::new_v4(),
                model_name: "test-model".to_string(),
                inference_type: "chat_completion_stream".to_string(),
                service_start_time: Instant::now(),
                provider_start_time: Instant::now(),
                first_token_received: false,
                first_token_time: None,
                ttft_ms: None,
                token_count: 0,
                last_token_time: None,
                total_itl_ms: 0.0,
                metric_tags: vec![],
                concurrent_counter: Some(counter.clone()),
                last_usage_stats: None,
                last_chat_id: None,
                stream_completed: false,
                response_id: None,
                last_finish_reason: None,
                last_error: None,
                state: StreamState::Streaming,
                attestation_supported: true,
            };
            // InterceptStream goes out of scope here and Drop is called
        }

        // Counter should be decremented to 0
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "Counter should be 0 after stream dropped"
        );
    }
}
