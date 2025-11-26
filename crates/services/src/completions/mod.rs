pub mod ports;

use crate::attestation::ports::AttestationServiceTrait;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::models::ModelsRepository;
use crate::usage::{RecordUsageServiceRequest, UsageServiceTrait};
use inference_providers::{ChatMessage, MessageRole, SSEEvent, StreamChunk, StreamingResult};
use std::sync::Arc;
use uuid::Uuid;

// Create a new stream that intercepts messages, but passes the original ones through
use crate::metrics::{consts::*, MetricsServiceTrait};
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

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
    request_type: String,
    start_time: Instant,
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
}

impl<S> Stream for InterceptStream<S>
where
    S: Stream<Item = Result<SSEEvent, inference_providers::CompletionError>> + Unpin,
{
    type Item = Result<SSEEvent, inference_providers::CompletionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(ref event))) => {
                let now = Instant::now();

                if !self.first_token_received {
                    self.first_token_received = true;
                    self.first_token_time = Some(now);
                    let duration = self.start_time.elapsed();
                    // Capture TTFT in milliseconds for DB storage
                    self.ttft_ms = Some(duration.as_millis() as i32);
                    self.last_token_time = Some(now);
                    // Reuse pre-allocated tags
                    let tags_str: Vec<&str> = self.metric_tags.iter().map(|s| s.as_str()).collect();
                    self.metrics_service
                        .record_latency(METRIC_LATENCY_TTFT, duration, &tags_str);
                } else if let Some(last_time) = self.last_token_time {
                    // Calculate inter-token latency
                    let itl = now.duration_since(last_time);
                    self.total_itl_ms += itl.as_secs_f64() * 1000.0;
                    self.token_count += 1;
                    self.last_token_time = Some(now);
                }

                if let StreamChunk::Chat(ref chat_chunk) = event.chunk {
                    if let Some(usage) = &chat_chunk.usage {
                        // Store attestation signature when completion finishes
                        let attestation_service = self.attestation_service.clone();
                        let chat_id = chat_chunk.id.clone();
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

                        // Calculate average ITL
                        let avg_itl_ms = if self.token_count > 0 {
                            Some(self.total_itl_ms / self.token_count as f64)
                        } else {
                            None
                        };

                        // Record usage with latency metrics
                        let usage_service = self.usage_service.clone();
                        let organization_id = self.organization_id;
                        let workspace_id = self.workspace_id;
                        let api_key_id = self.api_key_id;
                        let model_id = self.model_id;
                        let request_type = self.request_type.clone();
                        let input_tokens = usage.prompt_tokens;
                        let output_tokens = usage.completion_tokens;
                        let ttft_ms = self.ttft_ms;

                        tokio::spawn(async move {
                            if usage_service
                                .record_usage(RecordUsageServiceRequest {
                                    organization_id,
                                    workspace_id,
                                    api_key_id,
                                    response_id: None,
                                    model_id,
                                    input_tokens,
                                    output_tokens,
                                    request_type,
                                    ttft_ms,
                                    avg_itl_ms,
                                })
                                .await
                                .is_err()
                            {
                                tracing::error!("Failed to record usage in completion service");
                            } else {
                                tracing::debug!(
                                    "Recorded usage for org {}: {} input, {} output tokens (api_key: {}, ttft: {:?}ms)",
                                    organization_id,
                                    input_tokens,
                                    output_tokens,
                                    api_key_id,
                                    ttft_ms
                                );
                            }
                        });

                        // Record metrics
                        let metrics_service = self.metrics_service.clone();
                        let duration = self.start_time.elapsed();
                        let total_tokens = usage.completion_tokens;
                        let input_tokens = usage.prompt_tokens;
                        let output_tokens = usage.completion_tokens;
                        let first_token_time = self.first_token_time;
                        // Reuse pre-allocated tags
                        let tags_owned = self.metric_tags.clone();

                        tokio::spawn(async move {
                            let tags: Vec<&str> = tags_owned.iter().map(|s| s.as_str()).collect();

                            // Total latency
                            metrics_service.record_latency(METRIC_LATENCY_TOTAL, duration, &tags);

                            // Decoding time (first token to last token)
                            if let Some(first_token_instant) = first_token_time {
                                let decoding_duration = first_token_instant.elapsed();
                                metrics_service.record_latency(
                                    METRIC_LATENCY_DECODING_TIME,
                                    decoding_duration,
                                    &tags,
                                );
                            }

                            // Tokens per second
                            if duration.as_secs_f64() > 0.0 {
                                let tps = total_tokens as f64 / duration.as_secs_f64();
                                metrics_service.record_histogram(
                                    METRIC_TOKENS_PER_SECOND,
                                    tps,
                                    &tags,
                                );
                            }

                            // Token counts
                            metrics_service.record_count(
                                METRIC_TOKENS_INPUT,
                                input_tokens as i64,
                                &tags,
                            );
                            metrics_service.record_count(
                                METRIC_TOKENS_OUTPUT,
                                output_tokens as i64,
                                &tags,
                            );
                        });
                    }
                }
                Poll::Ready(Some(Ok(event.clone())))
            }
            other => other,
        }
    }
}

pub struct CompletionServiceImpl {
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub attestation_service: Arc<dyn AttestationServiceTrait>,
    pub usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
    pub metrics_service: Arc<dyn MetricsServiceTrait>,
    pub models_repository: Arc<dyn ModelsRepository>,
}

impl CompletionServiceImpl {
    pub fn new(
        inference_provider_pool: Arc<InferenceProviderPool>,
        attestation_service: Arc<dyn AttestationServiceTrait>,
        usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
        metrics_service: Arc<dyn MetricsServiceTrait>,
        models_repository: Arc<dyn ModelsRepository>,
    ) -> Self {
        Self {
            inference_provider_pool,
            attestation_service,
            usage_service,
            metrics_service,
            models_repository,
        }
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

    /// Record an error metric with the appropriate error type tag
    fn record_error(&self, error: &ports::CompletionError, model_name: Option<&str>) {
        let error_type = match error {
            ports::CompletionError::InvalidModel(_) => ERROR_TYPE_INVALID_MODEL,
            ports::CompletionError::InvalidParams(_) => ERROR_TYPE_INVALID_PARAMS,
            ports::CompletionError::RateLimitExceeded => ERROR_TYPE_RATE_LIMIT,
            ports::CompletionError::ProviderError(_) => ERROR_TYPE_INFERENCE_ERROR,
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
                content: Some(msg.content.clone()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            })
            .collect()
    }

    async fn handle_stream_with_context(
        &self,
        llm_stream: StreamingResult,
        organization_id: Uuid,
        workspace_id: Uuid,
        api_key_id: Uuid,
        model_id: Uuid,
        model_name: String,
        request_type: &str,
        request_start_time: Instant,
    ) -> StreamingResult {
        // Create low-cardinality metric tags (no org/workspace/key - those go to database)
        let metric_tags = Self::create_metric_tags(&model_name);

        // Record request count metric
        let tags_str: Vec<&str> = metric_tags.iter().map(|s| s.as_str()).collect();
        self.metrics_service
            .record_count(METRIC_REQUEST_COUNT, 1, &tags_str);

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
            request_type: request_type.to_string(),
            start_time: request_start_time,
            first_token_received: false,
            first_token_time: None,
            ttft_ms: None,
            token_count: 0,
            last_token_time: None,
            total_itl_ms: 0.0,
            metric_tags,
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
            response_format: None,
            seed: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: request.metadata,
            store: None,
            stream_options: None,
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

        // Capture start time BEFORE making the request to provider (for accurate TTFT)
        let request_start_time = Instant::now();

        // Get the LLM stream
        let llm_stream = match self
            .inference_provider_pool
            .chat_completion_stream(chat_params, request.body_hash.clone())
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                // Check if this is a client error (HTTP 4xx) from the provider
                let error_str = e.to_string();
                let err = if error_str.contains("HTTP 4") || error_str.contains("Bad Request") {
                    // For client errors (4xx), return detailed message to help user fix their request
                    ports::CompletionError::InvalidParams(format!(
                        "Invalid request parameters: {e}"
                    ))
                } else {
                    // For server errors (5xx), log details but return generic message to user
                    tracing::error!(
                        model = %request.model,
                        "Provider error during chat completion stream"
                    );
                    ports::CompletionError::ProviderError(
                        "The model is currently unavailable. Please try again later.".to_string(),
                    )
                };
                self.record_error(&err, Some(canonical_name));
                return Err(err);
            }
        };

        // Determine request type
        let request_type = if is_streaming {
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
                request_type,
                request_start_time,
            )
            .await;

        Ok(event_stream)
    }

    async fn create_chat_completion(
        &self,
        request: ports::CompletionRequest,
    ) -> Result<inference_providers::ChatCompletionResponseWithBytes, ports::CompletionError> {
        let start_time = Instant::now();
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
            response_format: None,
            seed: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: request.metadata,
            store: None,
            stream_options: None,
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

        let response_with_bytes = match self
            .inference_provider_pool
            .chat_completion(chat_params, request.body_hash.clone())
            .await
        {
            Ok(response) => response,
            Err(e) => {
                // Check if this is a client error (HTTP 4xx) from the provider
                let error_str = e.to_string();
                let err = if error_str.contains("HTTP 4") || error_str.contains("Bad Request") {
                    // For client errors (4xx), return detailed message to help user fix their request
                    ports::CompletionError::InvalidParams(format!(
                        "Invalid request parameters: {e}"
                    ))
                } else {
                    // For server errors (5xx), log details but return generic message to user
                    tracing::error!(
                        model = %request.model,
                        "Provider error during chat completion"
                    );
                    ports::CompletionError::ProviderError(
                        "The model is currently unavailable. Please try again later.".to_string(),
                    )
                };
                self.record_error(&err, Some(canonical_name));
                return Err(err);
            }
        };

        // For non-streaming, total latency = time until full response received
        let total_latency = start_time.elapsed();

        // Store attestation signature
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

        // Record metrics with low-cardinality tags only
        let metrics_service = self.metrics_service.clone();
        let total_tokens = response_with_bytes.response.usage.completion_tokens;
        let input_tokens = response_with_bytes.response.usage.prompt_tokens;
        let output_tokens = response_with_bytes.response.usage.completion_tokens;
        let model_name = model.model_name.clone();

        tokio::spawn(async move {
            // Create low-cardinality tags (model + environment only)
            let tags = CompletionServiceImpl::create_metric_tags(&model_name);
            let tags_str: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();

            // Request count
            metrics_service.record_count(METRIC_REQUEST_COUNT, 1, &tags_str);

            // For non-streaming, TTFT = total latency (all tokens arrive together)
            metrics_service.record_latency(METRIC_LATENCY_TTFT, total_latency, &tags_str);
            metrics_service.record_latency(METRIC_LATENCY_TOTAL, total_latency, &tags_str);

            // Tokens per second
            if total_latency.as_secs_f64() > 0.0 {
                let tps = total_tokens as f64 / total_latency.as_secs_f64();
                metrics_service.record_histogram(METRIC_TOKENS_PER_SECOND, tps, &tags_str);
            }

            // Token counts
            metrics_service.record_count(METRIC_TOKENS_INPUT, input_tokens as i64, &tags_str);
            metrics_service.record_count(METRIC_TOKENS_OUTPUT, output_tokens as i64, &tags_str);
        });

        // Record usage with model UUID
        // Note: TTFT doesn't apply to non-streaming (you get all tokens at once)
        let usage_service = self.usage_service.clone();
        let organization_id = request.organization_id;
        let workspace_id = request.workspace_id;
        let model_id = model.id;
        let input_tokens = response_with_bytes.response.usage.prompt_tokens;
        let output_tokens = response_with_bytes.response.usage.completion_tokens;

        tokio::spawn(async move {
            if usage_service
                .record_usage(RecordUsageServiceRequest {
                    organization_id,
                    workspace_id,
                    api_key_id,
                    response_id: None,
                    model_id,
                    input_tokens,
                    output_tokens,
                    request_type: "chat_completion".to_string(),
                    ttft_ms: None,    // N/A for non-streaming
                    avg_itl_ms: None, // N/A for non-streaming
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
}

pub use ports::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::capturing::{CapturingMetricsService, MetricValue};
    use crate::test_utils::{MockAttestationService, MockUsageService};
    use bytes::Bytes;
    use futures::{stream, StreamExt};
    use inference_providers::models::{ChatCompletionChunk, TokenUsage};
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
            }),
        };

        let usage_chunk = SSEEvent {
            raw_bytes: Bytes::from("data: ..."),
            chunk: StreamChunk::Chat(ChatCompletionChunk {
                id: "chat-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: "test-model".to_string(),
                choices: vec![],
                usage: Some(TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    total_tokens: 30,
                    prompt_tokens_details: None,
                }),
                prompt_token_ids: None,
                system_fingerprint: None,
            }),
        };

        let stream = stream::iter(vec![Ok(content_chunk), Ok(usage_chunk)]);

        let metric_tags = CompletionServiceImpl::create_metric_tags("test-model");

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
            request_type: "chat_completion_stream".to_string(),
            start_time: Instant::now(),
            first_token_received: false,
            first_token_time: None,
            ttft_ms: None,
            token_count: 0,
            last_token_time: None,
            total_itl_ms: 0.0,
            metric_tags,
        };

        // Consume the stream
        let _ = intercept_stream.collect::<Vec<_>>().await;

        // Verify metrics
        // Wait a bit for async tasks to complete
        tokio::time::sleep(Duration::from_millis(100)).await;

        let metrics = metrics_service.get_metrics();

        // Should have:
        // 1. latency.time_to_first_token (from first chunk)
        // 2. latency.total (from usage chunk)
        // 3. tokens_per_second (from usage chunk)
        assert!(
            metrics.len() >= 3,
            "Expected at least 3 metrics, got {}",
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
            }),
        };

        let usage_chunk = SSEEvent {
            raw_bytes: Bytes::from("data: usage"),
            chunk: StreamChunk::Chat(ChatCompletionChunk {
                id: "chat-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: "test-model".to_string(),
                choices: vec![],
                usage: Some(TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    total_tokens: 30,
                    prompt_tokens_details: None,
                }),
                prompt_token_ids: None,
                system_fingerprint: None,
            }),
        };

        // Simulate a stream with delays between chunks
        let stream = stream::iter(vec![Ok(chunk1), Ok(chunk2), Ok(usage_chunk)]);

        let metric_tags = CompletionServiceImpl::create_metric_tags("test-model");

        // Use a start time from "before" to simulate real TTFT
        let start_time = Instant::now() - Duration::from_millis(50);

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
            request_type: "chat_completion_stream".to_string(),
            start_time,
            first_token_received: false,
            first_token_time: None,
            ttft_ms: None,
            token_count: 0,
            last_token_time: None,
            total_itl_ms: 0.0,
            metric_tags,
        };

        // Consume the stream
        let _ = intercept_stream.collect::<Vec<_>>().await;

        // Wait for async usage recording to complete
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
                choices: vec![],
                usage: Some(TokenUsage {
                    prompt_tokens: 5,
                    completion_tokens: 1,
                    total_tokens: 6,
                    prompt_tokens_details: None,
                }),
                prompt_token_ids: None,
                system_fingerprint: None,
            }),
        };

        let stream = stream::iter(vec![Ok(usage_chunk)]);
        let metric_tags = CompletionServiceImpl::create_metric_tags("test-model");

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
            request_type: "chat_completion_stream".to_string(),
            start_time: Instant::now(),
            first_token_received: false,
            first_token_time: None,
            ttft_ms: None,
            token_count: 0,
            last_token_time: None,
            total_itl_ms: 0.0,
            metric_tags,
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
}
