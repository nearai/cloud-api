pub mod ports;

use crate::attestation::ports::AttestationServiceTrait;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::models::ModelsRepository;
use crate::responses::models::ResponseId;
use crate::usage::{RecordUsageServiceRequest, UsageServiceTrait};
use inference_providers::{ChatMessage, MessageRole, SSEEvent, StreamChunk, StreamingResult};
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
use tracing::Instrument;

const FINALIZE_TIMEOUT_SECS: u64 = 5;
const DEEPSEEK_V4_FLASH_MODEL: &str = "deepseek-ai/DeepSeek-V4-Flash";

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
    // IDs for usage tracking (database) and tracing
    request_id: Uuid,
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model_id: Uuid,
    #[allow(dead_code)] // Kept for potential debugging/logging use
    model_name: String,
    inference_type: crate::usage::ports::InferenceType,
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
    /// Flag indicating the stream completed normally (received None from inner stream).
    /// If false when Drop is called, the stream was interrupted — either the client
    /// disconnected mid-stream or the provider returned an error (check `last_error`).
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
    /// Whether to fetch/store provider chat signatures before ending the stream.
    store_provider_chat_signature: bool,
    provider_attribution: crate::usage::ProviderAttribution,
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
        if !self.attestation_supported || !self.store_provider_chat_signature {
            return Box::pin(async {});
        }

        let organization_id = self.organization_id;
        let model_id = self.model_id;

        let chat_id = match &self.last_chat_id {
            Some(id) => id.clone(),
            None => {
                tracing::warn!(%organization_id, %model_id, "Cannot store signature: no chat_id received in stream");
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
                    tracing::error!(%organization_id, %model_id, error = ?e, "Failed to store chat signature");
                }
                Err(_elapsed) => {
                    tracing::error!(
                        %organization_id,
                        %model_id,
                        "Timeout storing chat signature after {}s",
                        FINALIZE_TIMEOUT_SECS
                    );
                }
            }
        })
    }

    /// Record usage and metrics. Called from Drop to ensure it always runs.
    fn record_usage_and_metrics(&self) {
        let request_id = self.request_id;
        let organization_id = self.organization_id;
        let workspace_id = self.workspace_id;
        let api_key_id = self.api_key_id;
        let model_id = self.model_id;
        let inference_type = self.inference_type;

        // Create span with context BEFORE any early returns so all error logs have context
        let _span = tracing::error_span!(
            "stream_drop",
            %request_id,
            %organization_id,
            %workspace_id,
            %api_key_id,
            %model_id,
            %inference_type
        )
        .entered();

        let (input_tokens, output_tokens, cache_read_tokens, chat_id) = match (
            &self.last_usage_stats,
            &self.last_chat_id,
        ) {
            (Some(usage), Some(chat_id)) => (
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.cached_tokens(),
                chat_id.clone(),
            ),
            (None, None) => {
                // Distinguish client disconnect / provider error from truly unexpected cases.
                // Client disconnects and provider errors are expected — usage is only sent
                // in the final chunk, so an interrupted stream will never have it.
                if !self.stream_completed {
                    tracing::warn!(%organization_id, %model_id, stream_error = self.last_error.is_some(),
                        "Stream interrupted before usage stats or chat_id received (client disconnect or provider error)");
                } else {
                    tracing::error!(%organization_id, %model_id, "Stream completed but no usage stats and no chat_id available");
                }
                return;
            }
            (None, Some(chat_id)) => {
                if !self.stream_completed {
                    tracing::warn!(%chat_id, %organization_id, %model_id, stream_error = self.last_error.is_some(),
                        "Stream interrupted before usage stats received (client disconnect or provider error)");
                } else {
                    tracing::error!(%chat_id, %organization_id, %model_id, "Stream completed but no usage stats available");
                }
                return;
            }
            (Some(usage), None) => {
                tracing::error!(
                    prompt_tokens = usage.prompt_tokens,
                    completion_tokens = usage.completion_tokens,
                    %organization_id,
                    %model_id,
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
            %request_id,
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
        let provider_attribution = self.provider_attribution;

        let avg_itl_ms = if self.token_count > 0 {
            Some(self.total_itl_ms / self.token_count as f64)
        } else {
            None
        };

        let input_bucket = get_input_bucket(input_tokens);
        let mut metric_tags = self.metric_tags.clone();
        metric_tags.push(format!("{TAG_INPUT_BUCKET}:{input_bucket}"));

        // Spawn critical billing operations on blocking thread pool with timeout.
        // The tokio runtime waits for blocking tasks during graceful shutdown,
        // which helps prevent data loss compared to regular spawn.
        let handle_clone = handle.clone();
        handle.spawn_blocking(move || {
            handle_clone.block_on(
                async move {
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
                                cache_read_tokens,
                                inference_type,
                                ttft_ms,
                                avg_itl_ms,
                                inference_id: Some(inference_id),
                                provider_request_id: Some(chat_id),
                                stop_reason,
                                response_id,
                                image_count: None,
                                provider_attribution,
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
                                metrics_service.record_histogram(
                                    METRIC_TOKENS_PER_SECOND,
                                    tps,
                                    &tags,
                                );
                            }
                        }

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
                    })
                    .await;

                    if result.is_err() {
                        tracing::error!(
                            "Timeout recording usage and metrics (2s exceeded), inference_id={}",
                            inference_id
                        );
                    }
                }
                .instrument(span),
            )
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
                            // Control events (blank lines, comments, [DONE])
                            // carry no tokens: pass them through untouched so
                            // the route can forward their raw bytes, but keep
                            // them out of TTFT/ITL metrics and chat tracking.
                            if event.chunk.is_none() {
                                return Poll::Ready(Some(Ok(event.clone())));
                            }

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

                            if let Some(StreamChunk::Chat(ref chat_chunk)) = event.chunk {
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

/// RAII guard for concurrent request slots.
/// Automatically releases the slot when dropped, ensuring proper cleanup even if the request panics.
/// Use `disarm()` to take ownership of the counter without decrementing (e.g., to transfer it
/// to an `InterceptStream` that will handle decrement on drop).
struct ConcurrentSlotGuard {
    counter: Option<Arc<std::sync::atomic::AtomicU32>>,
}

impl ConcurrentSlotGuard {
    fn new(counter: Arc<AtomicU32>) -> Self {
        Self {
            counter: Some(counter),
        }
    }

    /// Disarm the guard and return the counter without decrementing.
    /// Used when transferring counter ownership to `InterceptStream`.
    fn disarm(&mut self) -> Option<Arc<AtomicU32>> {
        self.counter.take()
    }
}

impl Drop for ConcurrentSlotGuard {
    fn drop(&mut self) {
        if let Some(counter) = &self.counter {
            counter.fetch_sub(1, Ordering::Release);
        }
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
}

/// TTL for organization concurrent limit cache (5 minutes)
const ORG_LIMIT_CACHE_TTL_SECS: u64 = 300;

/// TTL for concurrent count cache entries (10 minutes).
/// Safety net: if a counter gets stuck (e.g., due to a panic or proxy not propagating
/// client disconnection), the entry expires and is replaced with a fresh zero counter.
///
/// Trade-off: if legitimate long-running requests are still in-flight when the TTL fires,
/// the limit can be temporarily exceeded until those old requests complete.
const CONCURRENT_COUNT_TTL_SECS: u64 = 600;

impl CompletionServiceImpl {
    /// Inject tracing correlation IDs into the `extra` map that is forwarded
    /// to `ChatCompletionParams`. The inference provider reads these keys and
    /// emits them as `X-Request-Id` / `X-Org-Id` / `X-Workspace-Id` HTTP
    /// headers on the outbound call to the vLLM/SGLang backend.
    ///
    /// The key names must match the constants in
    /// `inference_providers::attested::nearai::tracing_headers`.
    fn inject_tracing_headers(
        extra: &mut std::collections::HashMap<String, serde_json::Value>,
        request_id: Uuid,
        organization_id: Uuid,
        workspace_id: Uuid,
    ) {
        extra.insert(
            "x_request_id".to_string(),
            serde_json::Value::String(request_id.to_string()),
        );
        extra.insert(
            "x_org_id".to_string(),
            serde_json::Value::String(organization_id.to_string()),
        );
        extra.insert(
            "x_workspace_id".to_string(),
            serde_json::Value::String(workspace_id.to_string()),
        );
    }

    pub fn new(
        inference_provider_pool: Arc<InferenceProviderPool>,
        attestation_service: Arc<dyn AttestationServiceTrait>,
        usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
        metrics_service: Arc<dyn MetricsServiceTrait>,
        models_repository: Arc<dyn ModelsRepository>,
        organization_limit_repository: Arc<dyn ports::OrganizationConcurrentLimitRepository>,
    ) -> Self {
        let concurrent_counts = Cache::builder()
            .max_capacity(100_000)
            .time_to_live(Duration::from_secs(CONCURRENT_COUNT_TTL_SECS))
            .build();

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
        }
    }

    /// Extract tools and tool_choice from the extra HashMap if present and
    /// parseable as the typed `ToolDefinition` / `ToolChoice` shapes.
    ///
    /// Returns `(tools, tool_choice)` and only removes the corresponding
    /// key from `extra` when parsing succeeds. If parsing fails — for
    /// example because the client sent a non-standard tool type like
    /// NEAR's `{"type":"web_context_search"}`, which the upstream
    /// inference-proxy handles natively but doesn't fit the
    /// function-tool shape — the original value is left in `extra` so it
    /// flows through to the upstream request body verbatim via the
    /// `#[serde(flatten)]` on `ChatCompletionParams.extra`. This means
    /// cloud-api never silently drops tool definitions it doesn't
    /// recognise; the upstream gets to decide.
    fn extract_tools_from_extra(
        extra: &mut std::collections::HashMap<String, serde_json::Value>,
    ) -> (
        Option<Vec<inference_providers::ToolDefinition>>,
        Option<inference_providers::ToolChoice>,
    ) {
        let tools = match extra.get("tools").cloned() {
            Some(raw) => {
                match serde_json::from_value::<Vec<inference_providers::ToolDefinition>>(raw) {
                    Ok(parsed) => {
                        extra.remove("tools");
                        Some(parsed)
                    }
                    Err(_) => None,
                }
            }
            None => None,
        };

        let tool_choice = match extra.get("tool_choice").cloned() {
            Some(raw) => match serde_json::from_value::<inference_providers::ToolChoice>(raw) {
                Ok(parsed) => {
                    extra.remove("tool_choice");
                    Some(parsed)
                }
                Err(_) => None,
            },
            None => None,
        };

        // Honor `tool_choice: "none"` universally (nearai/cloud-api #619).
        //
        // OpenAI semantics: "none" forbids the model from calling any tool on
        // this turn. Some backends (notably vLLM-served Qwen / gpt-oss) ignore
        // `tool_choice` and emit tool_calls anyway. The robust, backend-agnostic
        // enforcement is to strip the tool definitions entirely so there is
        // nothing the model *can* call. We drop both the typed `tools` and any
        // `tools` left in `extra` (e.g. non-standard tool shapes that didn't
        // parse above). The `tool_choice: "none"` itself is harmless to forward.
        let tools = if matches!(
            tool_choice,
            Some(inference_providers::ToolChoice::String(ref s)) if s == "none"
        ) {
            extra.remove("tools");
            None
        } else {
            tools
        };

        (tools, tool_choice)
    }

    /// Extract typed OpenAI stream options from flattened request extras.
    /// Removing the parsed key avoids serializing duplicate `stream_options`
    /// fields once `ChatCompletionParams.stream_options` is populated.
    fn extract_stream_options_from_extra(
        extra: &mut std::collections::HashMap<String, serde_json::Value>,
    ) -> Option<inference_providers::StreamOptions> {
        match extra.get("stream_options").cloned() {
            Some(raw) => match serde_json::from_value::<inference_providers::StreamOptions>(raw) {
                Ok(parsed) => {
                    extra.remove("stream_options");
                    Some(parsed)
                }
                Err(_) => None,
            },
            None => None,
        }
    }

    fn is_json_object_response_format(
        extra: &std::collections::HashMap<String, serde_json::Value>,
    ) -> bool {
        extra
            .get("response_format")
            .and_then(|format| format.get("type"))
            .and_then(|kind| kind.as_str())
            == Some("json_object")
    }

    fn has_forced_function_tool_choice(
        tool_choice: &Option<inference_providers::ToolChoice>,
    ) -> bool {
        matches!(
            tool_choice,
            Some(inference_providers::ToolChoice::Function { .. })
        )
    }

    fn disable_chat_template_thinking(
        extra: &mut std::collections::HashMap<String, serde_json::Value>,
    ) -> bool {
        let Some(kwargs) = extra
            .get_mut("chat_template_kwargs")
            .and_then(|value| value.as_object_mut())
        else {
            return false;
        };

        let mut changed = false;
        for key in ["thinking", "enable_thinking"] {
            if kwargs.get(key).and_then(|value| value.as_bool()) == Some(true) {
                kwargs.insert(key.to_string(), serde_json::Value::Bool(false));
                changed = true;
            }
        }

        changed
    }

    fn apply_deepseek_v4_flash_thinking_compat(
        model_name: &str,
        params: &mut inference_providers::ChatCompletionParams,
    ) {
        if model_name != DEEPSEEK_V4_FLASH_MODEL {
            return;
        }

        let has_json_object = Self::is_json_object_response_format(&params.extra);
        let has_forced_tool_choice = Self::has_forced_function_tool_choice(&params.tool_choice);
        if !has_json_object && !has_forced_tool_choice {
            return;
        }

        if Self::disable_chat_template_thinking(&mut params.extra) {
            let reason = match (has_json_object, has_forced_tool_choice) {
                (true, true) => "json_object_and_forced_tool_choice",
                (true, false) => "json_object",
                (false, true) => "forced_tool_choice",
                (false, false) => unreachable!(),
            };
            tracing::warn!(
                model = model_name,
                reason,
                "Disabled DeepSeek-V4-Flash SGLang thinking for OpenAI-compatible response contract"
            );
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
    /// Reject E2EE requests for models that don't support attestation (external providers).
    fn reject_e2ee_if_unsupported(
        attestation_supported: bool,
        extra: &std::collections::HashMap<String, serde_json::Value>,
        model_name: &str,
    ) -> Result<(), ports::CompletionError> {
        if !attestation_supported {
            if let Some(pub_key) = extra.get(crate::common::encryption_headers::MODEL_PUB_KEY) {
                if pub_key.as_str().is_some() {
                    return Err(ports::CompletionError::InvalidModel(format!(
                        "Model '{}' does not support encryption. \
                         External providers run outside of our Trusted Execution Environment.",
                        model_name
                    )));
                }
            }
        }
        Ok(())
    }

    /// Reject `n > 1` requests for models that don't support multiple completions
    /// (i.e. external passthrough providers: Anthropic, Gemini, OpenAI, moonshotai).
    ///
    /// Self-hosted vLLM/SGLang models have `attestation_supported = true` and honour
    /// `n` natively. External providers silently return a single choice, so we surface
    /// the unsupported parameter as a client error (HTTP 400 / invalid_request_error)
    /// instead of silently dropping it.
    fn reject_n_gt_1_if_unsupported(
        attestation_supported: bool,
        n: Option<i64>,
        model_name: &str,
    ) -> Result<(), ports::CompletionError> {
        if !attestation_supported && n.is_some_and(|v| v > 1) {
            return Err(ports::CompletionError::InvalidParams(format!(
                "n>1 is not supported for model '{}'. \
                 External providers do not support multiple completions per request. \
                 Use a self-hosted model or make N separate requests.",
                model_name
            )));
        }
        Ok(())
    }

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

    pub(crate) fn map_provider_error(
        model: &str,
        error: &inference_providers::CompletionError,
        operation: &str,
        organization_id: Uuid,
    ) -> ports::CompletionError {
        match error {
            inference_providers::CompletionError::HttpError {
                status_code,
                message,
                is_external,
            } => match (*status_code, *is_external) {
                // --- Client errors that should be passed through (both internal and external) ---

                // 400 Bad Request = invalid params (context too long, bad format, etc.)
                (400, _) => {
                    tracing::warn!(%organization_id, model, status_code, "Client error during {}", operation);
                    ports::CompletionError::InvalidParams(message.clone())
                }
                // 413 Payload Too Large = client sent too much data
                (413, _) => {
                    tracing::warn!(%organization_id, model, status_code, "Payload too large during {}", operation);
                    ports::CompletionError::InvalidParams(message.clone())
                }
                // 422 Unprocessable Entity = invalid request content
                (422, _) => {
                    tracing::warn!(
                        %organization_id,
                        model,
                        status_code,
                        "Unprocessable entity during {}",
                        operation
                    );
                    ports::CompletionError::InvalidParams(message.clone())
                }
                // 429 Too Many Requests = rate limited
                (429, _) => {
                    tracing::warn!(%organization_id, model, status_code, "Rate limited during {}", operation);
                    ports::CompletionError::RateLimitExceeded(format!(
                        "Rate limit exceeded by upstream provider for model '{}'. Please retry with exponential backoff.",
                        model
                    ))
                }

                // --- Infrastructure errors that should be masked ---

                // 401/403 = auth errors from our infrastructure — never leak details
                (401 | 403, _) => {
                    tracing::error!(
                        %organization_id,
                        model,
                        status_code,
                        provider_message = %message,
                        "Auth error during {}",
                        operation
                    );
                    ports::CompletionError::ProviderError {
                        status_code: 500,
                        message: "The model is currently unavailable. Please try again later."
                            .to_string(),
                    }
                }
                // 404 from external provider = provider can't serve the model, not client's fault
                (404, true) => {
                    tracing::error!(
                        %organization_id,
                        model,
                        status_code,
                        provider_message = %message,
                        "External provider not found during {}",
                        operation
                    );
                    ports::CompletionError::ProviderError {
                        status_code: 502,
                        message: "The model is currently unavailable. Please try again later."
                            .to_string(),
                    }
                }
                // 404 from vLLM = model not found in our infrastructure
                (404, false) => {
                    tracing::warn!(%organization_id, model, status_code, "Not found during {}", operation);
                    ports::CompletionError::InvalidModel(message.clone())
                }
                // 408 Request Timeout = provider timed out
                (408, _) => {
                    tracing::error!(
                        %organization_id,
                        model,
                        status_code,
                        provider_message = %message,
                        "Provider timeout during {}",
                        operation
                    );
                    ports::CompletionError::ProviderError {
                        status_code: 504,
                        message: "The request timed out. Please try again.".to_string(),
                    }
                }
                // 503 Service Unavailable = service overloaded
                (503, _) => {
                    tracing::warn!(
                        %organization_id,
                        model,
                        status_code,
                        "Service overloaded during {}",
                        operation
                    );
                    ports::CompletionError::ServiceOverloaded(
                        "The service is temporarily overloaded. Please retry with exponential backoff.".to_string(),
                    )
                }
                // 504 Gateway Timeout = TTFB timeout waiting for our vLLM infrastructure
                (504, false) => {
                    tracing::error!(
                        %organization_id,
                        model,
                        status_code,
                        "TTFB timeout waiting for inference backend during {}",
                        operation
                    );
                    ports::CompletionError::ProviderError {
                        status_code: 504,
                        message: "The request timed out waiting for the model to respond. Please try again.".to_string(),
                    }
                }
                // 5xx = provider error, use generic message
                (500..=599, _) => {
                    tracing::error!(
                        %organization_id,
                        model,
                        status_code,
                        provider_message = %message,
                        "Provider error during {}",
                        operation
                    );
                    ports::CompletionError::ProviderError {
                        status_code: 502,
                        message: "The model is currently unavailable. Please try again later."
                            .to_string(),
                    }
                }
                // Any other external provider error = infrastructure problem, use generic message
                _ if *is_external => {
                    tracing::error!(
                        %organization_id,
                        model,
                        status_code,
                        provider_message = %message,
                        "External provider error during {}",
                        operation
                    );
                    ports::CompletionError::ProviderError {
                        status_code: 502,
                        message: "The model is currently unavailable. Please try again later."
                            .to_string(),
                    }
                }
                // Any other vLLM error = pass through status and message
                _ => {
                    tracing::warn!(%organization_id, model, status_code, "Provider error during {}", operation);
                    ports::CompletionError::ProviderError {
                        status_code: *status_code,
                        message: message.clone(),
                    }
                }
            },
            // The pool already determined (on the RAW upstream body, before URL
            // redaction) that the engine couldn't fetch/decode a client-supplied
            // image/video. Surface a 400 (non-retryable) — not a 502 — and use a
            // generic message: the carried body holds the user's URL and internal
            // paths, which must not be echoed to the client or logged here.
            inference_providers::CompletionError::ClientMediaError(_) => {
                tracing::warn!(
                    %organization_id,
                    model,
                    "Client media fetch/decode error during {}",
                    operation
                );
                ports::CompletionError::InvalidParams(
                    "One or more image or video inputs could not be fetched or decoded. \
                     Ensure each URL is reachable and resolves to a valid image or video."
                        .to_string(),
                )
            }
            inference_providers::CompletionError::NoPubKeyProvider(msg) => {
                tracing::warn!(
                    model,
                    provider_message = %msg,
                    "E2EE pubkey routing failed during {} (stale attestation?)",
                    operation
                );
                ports::CompletionError::ProviderError {
                    status_code: 421,
                    message: "The encryption key is no longer valid. Please refresh your attestation report and retry.".to_string(),
                }
            }
            inference_providers::CompletionError::CompletionError(msg) => {
                if msg.contains("not found in any configured provider") {
                    ports::CompletionError::InvalidModel(msg.clone())
                } else {
                    tracing::error!(
                        %organization_id,
                        model,
                        provider_message = %msg,
                        "Provider error during {}",
                        operation
                    );
                    ports::CompletionError::ProviderError {
                        status_code: 502,
                        message: "The model is currently unavailable. Please try again later."
                            .to_string(),
                    }
                }
            }
            inference_providers::CompletionError::InvalidResponse(msg) => {
                tracing::error!(
                    %organization_id,
                    model,
                    provider_message = %msg,
                    "Invalid response during {}",
                    operation
                );
                ports::CompletionError::ProviderError {
                    status_code: 502,
                    message: "The model is currently unavailable. Please try again later."
                        .to_string(),
                }
            }
            inference_providers::CompletionError::Unknown(msg) => {
                tracing::error!(
                    %organization_id,
                    model,
                    provider_message = %msg,
                    "Unknown error during {}",
                    operation
                );
                ports::CompletionError::ProviderError {
                    status_code: 502,
                    message: "The model is currently unavailable. Please try again later."
                        .to_string(),
                }
            }
            inference_providers::CompletionError::Timeout {
                operation: op,
                timeout_seconds,
            } => {
                tracing::error!(
                    %organization_id,
                    model,
                    timeout_seconds,
                    inference_op = op,
                    "Provider per-call timeout during {}",
                    operation
                );
                ports::CompletionError::ProviderError {
                    status_code: 504,
                    message:
                        "The request timed out waiting for the model to respond. Please try again."
                            .to_string(),
                }
            }
        }
    }

    /// Record an error metric with the appropriate error type tag
    fn record_error(&self, error: &ports::CompletionError, model_name: Option<&str>) {
        let error_type = match error {
            ports::CompletionError::InvalidModel(_) => ERROR_TYPE_INVALID_MODEL,
            ports::CompletionError::InvalidParams(_) => ERROR_TYPE_INVALID_PARAMS,
            ports::CompletionError::RateLimitExceeded(_) => ERROR_TYPE_RATE_LIMIT,
            ports::CompletionError::ProviderError { .. } => ERROR_TYPE_INFERENCE_ERROR,
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
            .map(|msg| {
                // Convert tool_calls from CompletionToolCall to inference_providers::models::ToolCall
                let tool_calls = msg.tool_calls.as_ref().map(|calls| {
                    calls
                        .iter()
                        .map(|tc| inference_providers::models::ToolCall {
                            id: Some(tc.id.clone()),
                            type_: Some("function".to_string()),
                            function: inference_providers::models::FunctionCall {
                                name: Some(tc.name.clone()),
                                arguments: Some(tc.arguments.clone()),
                            },
                            index: None,
                            thought_signature: tc.thought_signature.clone(),
                        })
                        .collect()
                });

                ChatMessage {
                    role: match msg.role.as_str() {
                        "system" | "developer" => MessageRole::System,
                        "assistant" => MessageRole::Assistant,
                        "tool" => MessageRole::Tool,
                        _ => MessageRole::User,
                    },
                    content: Some(msg.content.clone()),
                    name: None,
                    tool_call_id: msg.tool_call_id.clone(),
                    tool_calls,
                }
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
                let msg = format!(
                    "Concurrent request limit exceeded for model {}. Organization limit: {} concurrent requests per model.",
                    model_name, limit
                );
                self.record_error(
                    &ports::CompletionError::RateLimitExceeded(msg.clone()),
                    Some(model_name),
                );
                return Err(ports::CompletionError::RateLimitExceeded(msg));
            }
            if counter
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(counter);
            }
        }
    }

    // CLIPPY-ALLOW: InterceptStream construction needs the full tracing, billing, timing, and concurrency context at one ownership boundary.
    #[allow(clippy::too_many_arguments)]
    async fn handle_stream_with_context(
        &self,
        llm_stream: StreamingResult,
        request_id: Uuid,
        organization_id: Uuid,
        workspace_id: Uuid,
        api_key_id: Uuid,
        model_id: Uuid,
        model_name: String,
        inference_type: crate::usage::ports::InferenceType,
        service_start_time: Instant,
        provider_start_time: Instant,
        concurrent_counter: Option<Arc<AtomicU32>>,
        response_id: Option<ResponseId>,
        attestation_supported: bool,
        store_provider_chat_signature: bool,
        provider_attribution: crate::usage::ProviderAttribution,
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
            request_id,
            organization_id,
            workspace_id,
            api_key_id,
            model_id,
            model_name,
            inference_type,
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
            store_provider_chat_signature,
            provider_attribution,
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
        let request_id = request.request_id;
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

        // Extract tools from extra if present (Responses API puts them there)
        let mut extra = request.extra.clone();
        let (tools, tool_choice) = Self::extract_tools_from_extra(&mut extra);
        let stream_options = Self::extract_stream_options_from_extra(&mut extra);

        // Inject tracing correlation IDs into extra so the inference provider
        // forwards them as X-Request-Id / X-Org-Id / X-Workspace-Id headers.
        Self::inject_tracing_headers(&mut extra, request_id, organization_id, workspace_id);

        let mut chat_params = inference_providers::ChatCompletionParams {
            model: request.model.clone(),
            messages: chat_messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            top_p: request.top_p,
            stop: request.stop,
            stream: Some(true),
            tools,
            max_completion_tokens: None,
            n: request.n,
            frequency_penalty: None,
            presence_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: Some(request.user_id.to_string()),
            seed: None,
            tool_choice,
            parallel_tool_calls: None,
            // Drop metadata if store is not explicitly enabled (OpenAI requirement)
            metadata: if request.store == Some(true) {
                request.metadata.clone()
            } else {
                None
            },
            store: request.store,
            stream_options,
            modalities: None,
            extra,
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
        Self::apply_deepseek_v4_flash_thinking_compat(canonical_name, &mut chat_params);

        let counter = self
            .try_acquire_concurrent_slot(organization_id, model.id, canonical_name)
            .await?;

        // RAII guard protects against panics during stream creation.
        // On success, disarm and transfer counter ownership to InterceptStream.
        let mut guard = ConcurrentSlotGuard::new(counter);

        Self::reject_e2ee_if_unsupported(
            model.attestation_supported,
            &chat_params.extra,
            canonical_name,
        )?;

        Self::reject_n_gt_1_if_unsupported(model.attestation_supported, request.n, canonical_name)?;

        let provider_start_time = Instant::now();

        // Get the LLM stream
        let attributed_stream = match self
            .inference_provider_pool
            .chat_completion_stream_with_attribution(chat_params, request.body_hash.clone())
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                // Guard will decrement counter on drop
                let err = Self::map_provider_error(
                    &request.model,
                    &e,
                    "chat completion stream",
                    organization_id,
                );
                self.record_error(&err, Some(canonical_name));
                return Err(err);
            }
        };
        let llm_stream = attributed_stream.stream;
        let provider_attribution = attributed_stream.provider_attribution;

        // Transfer counter ownership to InterceptStream (which decrements on drop)
        let counter = guard.disarm();

        let inference_type = if is_streaming {
            crate::usage::ports::InferenceType::ChatCompletionStream
        } else {
            crate::usage::ports::InferenceType::ChatCompletion
        };

        // Create the completion event stream with usage tracking
        // Use model UUID for usage tracking, model name for low-cardinality metrics
        let event_stream = self
            .handle_stream_with_context(
                llm_stream,
                request_id,
                organization_id,
                workspace_id,
                api_key_id,
                model.id,
                model.model_name.clone(),
                inference_type,
                service_start_time,
                provider_start_time,
                counter,
                request.response_id,
                model.attestation_supported,
                !request.skip_provider_chat_signature,
                provider_attribution,
            )
            .await;

        Ok(event_stream)
    }

    async fn create_chat_completion(
        &self,
        request: ports::CompletionRequest,
    ) -> Result<inference_providers::ChatCompletionResponseWithBytes, ports::CompletionError> {
        let service_start_time = Instant::now();
        let organization_id = request.organization_id;
        let workspace_id = request.workspace_id;
        let request_id = request.request_id;
        let chat_messages = Self::prepare_chat_messages(&request.messages);

        // Extract tools from extra if present (Responses API puts them there)
        let mut extra = request.extra.clone();
        let (tools, tool_choice) = Self::extract_tools_from_extra(&mut extra);
        let stream_options = Self::extract_stream_options_from_extra(&mut extra);

        // Inject tracing correlation IDs into extra so the inference provider
        // forwards them as X-Request-Id / X-Org-Id / X-Workspace-Id headers.
        Self::inject_tracing_headers(&mut extra, request_id, organization_id, workspace_id);

        let mut chat_params = inference_providers::ChatCompletionParams {
            model: request.model.clone(),
            messages: chat_messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            top_p: request.top_p,
            stop: request.stop,
            stream: Some(false),
            tools,
            max_completion_tokens: None,
            n: request.n,
            frequency_penalty: None,
            presence_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: Some(request.user_id.to_string()),
            seed: None,
            tool_choice,
            parallel_tool_calls: None,
            // Drop metadata if store is not explicitly enabled (OpenAI requirement)
            metadata: if request.store == Some(true) {
                request.metadata.clone()
            } else {
                None
            },
            store: request.store,
            stream_options,
            modalities: None,
            extra,
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
        Self::apply_deepseek_v4_flash_thinking_compat(canonical_name, &mut chat_params);

        let organization_id = request.organization_id;
        let counter = self
            .try_acquire_concurrent_slot(organization_id, model.id, canonical_name)
            .await?;

        // RAII guard ensures slot is released on drop (panic, error, or success)
        let _guard = ConcurrentSlotGuard::new(counter);

        Self::reject_e2ee_if_unsupported(
            model.attestation_supported,
            &chat_params.extra,
            canonical_name,
        )?;

        Self::reject_n_gt_1_if_unsupported(model.attestation_supported, request.n, canonical_name)?;

        let provider_start_time = Instant::now();
        let result = self
            .inference_provider_pool
            .chat_completion_with_attribution(chat_params, request.body_hash.clone())
            .await;

        let attributed_response = match result {
            Ok(response) => response,
            Err(e) => {
                let err = Self::map_provider_error(
                    &request.model,
                    &e,
                    "chat completion",
                    organization_id,
                );
                self.record_error(&err, Some(canonical_name));
                return Err(err);
            }
        };
        let response_with_bytes = attributed_response.response;
        let provider_attribution = attributed_response.provider_attribution;

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
        let cache_read_tokens = response_with_bytes.response.usage.cached_tokens();
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
                    cache_read_tokens,
                    inference_type: crate::usage::ports::InferenceType::ChatCompletion,
                    ttft_ms: None,    // N/A for non-streaming
                    avg_itl_ms: None, // N/A for non-streaming
                    inference_id: Some(inference_id),
                    provider_request_id: Some(provider_request_id),
                    stop_reason: Some(stop_reason),
                    response_id,
                    image_count: None,
                    provider_attribution,
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

    async fn audio_transcription(
        &self,
        organization_id: uuid::Uuid,
        model_id: uuid::Uuid,
        model_name: &str,
        params: inference_providers::AudioTranscriptionParams,
        request_hash: String,
    ) -> Result<inference_providers::AudioTranscriptionResponse, ports::CompletionError> {
        // Acquire concurrent request slot to enforce organization limits
        let counter = self
            .try_acquire_concurrent_slot(organization_id, model_id, model_name)
            .await?;

        // RAII guard ensures slot is released on drop (panic, error, or success)
        let _guard = ConcurrentSlotGuard::new(counter);

        // Call inference provider pool with timeout protection
        let timeout_duration = std::time::Duration::from_secs(120); // 2 minute timeout for audio
        let result = tokio::time::timeout(
            timeout_duration,
            self.inference_provider_pool
                .audio_transcription(params, request_hash),
        )
        .await;

        // Handle timeout and map provider errors
        match result {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(e)) => match e {
                inference_providers::AudioTranscriptionError::TranscriptionError(msg) => {
                    Err(ports::CompletionError::ProviderError {
                        status_code: 502,
                        message: msg,
                    })
                }
                inference_providers::AudioTranscriptionError::HttpError {
                    status_code,
                    message,
                } => {
                    let mapped_status = match status_code {
                        401 | 403 => 500,
                        429 => {
                            return Err(ports::CompletionError::RateLimitExceeded(
                                "Rate limit exceeded by upstream provider. Please retry with exponential backoff.".to_string(),
                            ));
                        }
                        503 => {
                            return Err(ports::CompletionError::ServiceOverloaded(message));
                        }
                        500..=599 => 502,
                        other => other,
                    };
                    Err(ports::CompletionError::ProviderError {
                        status_code: mapped_status,
                        message,
                    })
                }
            },
            Err(_) => Err(ports::CompletionError::ProviderError {
                status_code: 504,
                message: "Audio transcription request timed out".to_string(),
            }),
        }
    }

    async fn try_rerank(
        &self,
        organization_id: Uuid,
        model_id: Uuid,
        model_name: &str,
        params: inference_providers::RerankParams,
    ) -> Result<inference_providers::RerankResponse, ports::CompletionError> {
        // Acquire concurrent request slot to enforce organization limits
        let counter = self
            .try_acquire_concurrent_slot(organization_id, model_id, model_name)
            .await?;

        // Create RAII guard to ensure slot is released on drop (panic, error, or success)
        let _guard = ConcurrentSlotGuard::new(counter);

        // Call inference provider pool
        // The guard will automatically release the slot when this function returns or panics
        let result = self.inference_provider_pool.rerank(params).await;

        // Map provider errors to service errors with proper status codes
        result.map_err(|e| match e {
            inference_providers::RerankError::GenerationError(msg) => {
                ports::CompletionError::ProviderError {
                    status_code: 502,
                    message: msg,
                }
            }
            inference_providers::RerankError::HttpError {
                status_code,
                message,
            } => match status_code {
                401 | 403 => ports::CompletionError::ProviderError {
                    status_code: 500,
                    message: "The model is currently unavailable. Please try again later."
                        .to_string(),
                },
                429 => ports::CompletionError::RateLimitExceeded(
                    "Rate limit exceeded by upstream provider. Please retry with exponential backoff.".to_string(),
                ),
                503 => ports::CompletionError::ServiceOverloaded(message),
                500..=599 => ports::CompletionError::ProviderError {
                    status_code: 502,
                    message,
                },
                other => ports::CompletionError::ProviderError {
                    status_code: other,
                    message,
                },
            },
        })
    }

    async fn try_embeddings(
        &self,
        organization_id: Uuid,
        model_id: Uuid,
        model_name: &str,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, ports::CompletionError> {
        let counter = self
            .try_acquire_concurrent_slot(organization_id, model_id, model_name)
            .await?;
        let _guard = ConcurrentSlotGuard::new(counter);

        self.inference_provider_pool
            .embeddings(model_name, body, extra)
            .await
            .map_err(|e| match e {
                inference_providers::EmbeddingError::RequestFailed(msg) => {
                    ports::CompletionError::ProviderError {
                        status_code: 502,
                        message: msg,
                    }
                }
                inference_providers::EmbeddingError::HttpError {
                    status_code,
                    message,
                } => match status_code {
                    401 | 403 => ports::CompletionError::ProviderError {
                        status_code: 500,
                        message: "The model is currently unavailable. Please try again later."
                            .to_string(),
                    },
                    429 => ports::CompletionError::RateLimitExceeded(
                        "Rate limit exceeded by upstream provider. Please retry with exponential backoff.".to_string(),
                    ),
                    503 => ports::CompletionError::ServiceOverloaded(message),
                    500..=599 => ports::CompletionError::ProviderError {
                        status_code: 502,
                        message,
                    },
                    other => ports::CompletionError::ProviderError {
                        status_code: other,
                        message,
                    },
                },
            })
    }

    async fn try_privacy_classify(
        &self,
        organization_id: Uuid,
        model_id: Uuid,
        model_name: &str,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, ports::CompletionError> {
        let counter = self
            .try_acquire_concurrent_slot(organization_id, model_id, model_name)
            .await?;
        let _guard = ConcurrentSlotGuard::new(counter);

        self.inference_provider_pool
            .privacy_classify(model_name, body, extra)
            .await
            .map_err(|e| match e {
                inference_providers::PrivacyClassifyError::RequestFailed(msg) => {
                    ports::CompletionError::ProviderError {
                        status_code: 502,
                        message: msg,
                    }
                }
                inference_providers::PrivacyClassifyError::HttpError {
                    status_code,
                    message,
                } => match status_code {
                    401 | 403 => ports::CompletionError::ProviderError {
                        status_code: 500,
                        message: "The model is currently unavailable. Please try again later."
                            .to_string(),
                    },
                    429 => ports::CompletionError::RateLimitExceeded(
                        "Rate limit exceeded by upstream provider. Please retry with exponential backoff.".to_string(),
                    ),
                    503 => ports::CompletionError::ServiceOverloaded(message),
                    500..=599 => ports::CompletionError::ProviderError {
                        status_code: 502,
                        message,
                    },
                    other => ports::CompletionError::ProviderError {
                        status_code: other,
                        message,
                    },
                },
            })
    }

    async fn try_score(
        &self,
        organization_id: Uuid,
        model_id: Uuid,
        model_name: &str,
        request_hash: String,
        params: inference_providers::ScoreParams,
    ) -> Result<inference_providers::ScoreResponse, ports::CompletionError> {
        // Acquire concurrent request slot to enforce organization limits
        let counter = self
            .try_acquire_concurrent_slot(organization_id, model_id, model_name)
            .await?;

        // Create RAII guard to ensure slot is released on drop (panic, error, or success)
        let _guard = ConcurrentSlotGuard::new(counter);

        // Call inference provider pool
        // The guard will automatically release the slot when this function returns or panics
        let result = self
            .inference_provider_pool
            .score(params, request_hash)
            .await;

        // Map provider errors to service errors with proper status codes
        result.map_err(|e| match e {
            inference_providers::ScoreError::GenerationError(msg) => {
                ports::CompletionError::ProviderError {
                    status_code: 502,
                    message: msg,
                }
            }
            inference_providers::ScoreError::HttpError {
                status_code,
                message,
            } => match status_code {
                401 | 403 => ports::CompletionError::ProviderError {
                    status_code: 500,
                    message: "The model is currently unavailable. Please try again later."
                        .to_string(),
                },
                429 => ports::CompletionError::RateLimitExceeded(
                    "Rate limit exceeded by upstream provider. Please retry with exponential backoff.".to_string(),
                ),
                503 => ports::CompletionError::ServiceOverloaded(message),
                500..=599 => ports::CompletionError::ProviderError {
                    status_code: 502,
                    message,
                },
                other => ports::CompletionError::ProviderError {
                    status_code: other,
                    message,
                },
            },
        })
    }

    async fn get_model(
        &self,
        model_name: &str,
    ) -> Result<Option<crate::models::ModelWithPricing>, anyhow::Error> {
        self.models_repository.get_model_by_name(model_name).await
    }

    fn get_inference_provider_pool(
        &self,
    ) -> std::sync::Arc<crate::inference_provider_pool::InferenceProviderPool> {
        self.inference_provider_pool.clone()
    }

    async fn invalidate_org_concurrent_limit(&self, org_id: Uuid) {
        self.org_concurrent_limits.invalidate(&org_id).await;
    }
}

pub use ports::*;

#[cfg(test)]
mod provider_attribution_tests;

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
            raw_passthrough: true,
            chunk: Some(StreamChunk::Chat(ChatCompletionChunk {
                id: "chat-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: "test-model".to_string(),
                choices: vec![],
                usage: None,
                prompt_token_ids: None,
                system_fingerprint: None,
                modality: None,
                extra: Default::default(),
            })),
        };

        let usage_chunk = SSEEvent {
            raw_bytes: Bytes::from("data: ..."),
            raw_passthrough: true,
            chunk: Some(StreamChunk::Chat(ChatCompletionChunk {
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
                extra: Default::default(),
            })),
        };

        let stream = stream::iter(vec![Ok(content_chunk), Ok(usage_chunk)]);

        let metric_tags = CompletionServiceImpl::create_metric_tags("test-model");

        let now = Instant::now();
        let intercept_stream = InterceptStream {
            inner: stream,
            attestation_service,
            usage_service,
            metrics_service: metrics_service.clone(),
            request_id: Uuid::new_v4(),
            organization_id,
            workspace_id,
            api_key_id,
            model_id,
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
            store_provider_chat_signature: true,
            provider_attribution: crate::usage::ProviderAttribution::default(),
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
            raw_passthrough: true,
            chunk: Some(StreamChunk::Chat(ChatCompletionChunk {
                id: "chat-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: "test-model".to_string(),
                choices: vec![],
                usage: None,
                prompt_token_ids: None,
                system_fingerprint: None,
                modality: None,
                extra: Default::default(),
            })),
        };

        let chunk2 = SSEEvent {
            raw_bytes: Bytes::from("data: chunk2"),
            raw_passthrough: true,
            chunk: Some(StreamChunk::Chat(ChatCompletionChunk {
                id: "chat-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: "test-model".to_string(),
                choices: vec![],
                usage: None,
                prompt_token_ids: None,
                system_fingerprint: None,
                modality: None,
                extra: Default::default(),
            })),
        };

        let usage_chunk = SSEEvent {
            raw_bytes: Bytes::from("data: usage"),
            raw_passthrough: true,
            chunk: Some(StreamChunk::Chat(ChatCompletionChunk {
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
                extra: Default::default(),
            })),
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
            request_id: Uuid::new_v4(),
            organization_id,
            workspace_id,
            api_key_id,
            model_id,
            model_name: "test-model".to_string(),
            inference_type: crate::usage::ports::InferenceType::ChatCompletionStream,
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
            store_provider_chat_signature: true,
            provider_attribution: crate::usage::ProviderAttribution::default(),
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
            raw_passthrough: true,
            chunk: Some(StreamChunk::Chat(ChatCompletionChunk {
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
                extra: Default::default(),
            })),
        };

        let stream = stream::iter(vec![Ok(usage_chunk)]);
        let metric_tags = CompletionServiceImpl::create_metric_tags("test-model");

        let now = Instant::now();
        let intercept_stream = InterceptStream {
            inner: stream,
            attestation_service,
            usage_service: usage_service.clone(),
            metrics_service: metrics_service.clone(),
            request_id: Uuid::new_v4(),
            organization_id,
            workspace_id,
            api_key_id,
            model_id,
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
            store_provider_chat_signature: true,
            provider_attribution: crate::usage::ProviderAttribution::default(),
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

    /// Mirrors the cache shape used by `CompletionServiceImpl::org_concurrent_limits`
    /// and `get_org_concurrent_limit`: `moka::future::Cache<Uuid, u32>` populated
    /// via `get_with` (load-on-miss with the closure return becoming the cached
    /// value). Asserts that `Cache::invalidate(&key)` forces the next `get_with`
    /// call to re-run its loader — which is exactly the contract
    /// `invalidate_org_concurrent_limit` relies on for admin PATCHes to take
    /// effect immediately instead of waiting for the 5-minute TTL.
    #[tokio::test]
    async fn test_org_concurrent_limit_cache_invalidates() {
        let cache: Cache<Uuid, u32> = Cache::builder()
            .time_to_live(Duration::from_secs(ORG_LIMIT_CACHE_TTL_SECS))
            .max_capacity(10_000)
            .build();

        let org_id = Uuid::new_v4();

        // First load: repo returns 64 (default).
        let v = cache.get_with(org_id, async { 64u32 }).await;
        assert_eq!(v, 64);

        // Second call with a different loader return — should still hit the
        // cached 64 because the entry is alive.
        let v = cache.get_with(org_id, async { 2u32 }).await;
        assert_eq!(
            v, 64,
            "stale cached value should survive without invalidate"
        );

        // Simulate admin PATCH writing a new limit to the DB and the service
        // invalidating the cache. After this, the next get_with must re-load.
        cache.invalidate(&org_id).await;

        let v = cache.get_with(org_id, async { 2u32 }).await;
        assert_eq!(
            v, 2,
            "after invalidate, next get_with should run the loader and pick up the new value"
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
                request_id: Uuid::new_v4(),
                organization_id: Uuid::new_v4(),
                workspace_id: Uuid::new_v4(),
                api_key_id: Uuid::new_v4(),
                model_id: Uuid::new_v4(),
                model_name: "test-model".to_string(),
                inference_type: crate::usage::ports::InferenceType::ChatCompletionStream,
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
                store_provider_chat_signature: true,
                provider_attribution: crate::usage::ProviderAttribution::default(),
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

    // ============================================
    // vLLM error mapping tests (is_external: false)
    // ============================================

    #[test]
    fn test_map_provider_error_400_preserves_message() {
        let error = inference_providers::CompletionError::HttpError {
            status_code: 400,
            message: "max_tokens must be positive".to_string(),
            is_external: false,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::InvalidParams(msg) => {
                assert!(
                    msg.contains("max_tokens must be positive"),
                    "Message should be preserved, got: {}",
                    msg
                );
            }
            other => panic!("Expected InvalidParams, got {:?}", other),
        }
    }

    #[test]
    fn test_map_provider_error_404_becomes_invalid_model() {
        let error = inference_providers::CompletionError::HttpError {
            status_code: 404,
            message: "Model 'deepseek-ai/DeepSeek-V3.1' not found".to_string(),
            is_external: false,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::InvalidModel(msg) => {
                assert!(
                    msg.contains("DeepSeek-V3.1"),
                    "Message should be preserved, got: {}",
                    msg
                );
            }
            other => panic!("Expected InvalidModel, got {:?}", other),
        }
    }

    #[test]
    fn test_map_provider_error_429_becomes_rate_limited() {
        let error = inference_providers::CompletionError::HttpError {
            status_code: 429,
            message: "Too many requests".to_string(),
            is_external: false,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        assert!(
            matches!(result, ports::CompletionError::RateLimitExceeded(_)),
            "Expected RateLimitExceeded, got {:?}",
            result
        );
    }

    #[test]
    fn test_map_provider_error_401_masks_auth_details() {
        let error = inference_providers::CompletionError::HttpError {
            status_code: 401,
            message: "Invalid API key for vLLM server at 10.0.0.1".to_string(),
            is_external: false,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::ProviderError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 500, "Auth errors should map to 500");
                assert!(
                    !message.contains("API key"),
                    "Should not expose auth details"
                );
                assert!(
                    !message.contains("10.0.0.1"),
                    "Should not expose internal IPs"
                );
            }
            other => panic!("Expected ProviderError, got {:?}", other),
        }
    }

    #[test]
    fn test_map_provider_error_503_becomes_service_overloaded() {
        let error = inference_providers::CompletionError::HttpError {
            status_code: 503,
            message: "Service temporarily overloaded".to_string(),
            is_external: false,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::ServiceOverloaded(msg) => {
                assert!(
                    msg.contains("overloaded"),
                    "Should indicate overloaded status, got: {}",
                    msg
                );
            }
            other => panic!("Expected ServiceOverloaded, got {:?}", other),
        }
    }

    #[test]
    fn test_map_provider_error_500_becomes_502() {
        let error = inference_providers::CompletionError::HttpError {
            status_code: 500,
            message: "Internal server error from provider".to_string(),
            is_external: false,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::ProviderError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 502, "Provider 500 should map to 502");
                assert!(
                    !message.contains("Internal server error from provider"),
                    "Should not expose provider error details, got: {}",
                    message
                );
                assert!(
                    message.contains("unavailable"),
                    "Should use generic message, got: {}",
                    message
                );
            }
            other => panic!("Expected ProviderError, got {:?}", other),
        }
    }

    #[test]
    fn test_map_provider_error_model_not_found_string() {
        let error = inference_providers::CompletionError::CompletionError(
            "Model 'test-model' not found in any configured provider".to_string(),
        );
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::InvalidModel(msg) => {
                assert!(
                    msg.contains("not found"),
                    "Message should be preserved, got: {}",
                    msg
                );
            }
            other => panic!("Expected InvalidModel, got {:?}", other),
        }
    }

    #[test]
    fn test_map_provider_error_connection_error_becomes_502() {
        let error = inference_providers::CompletionError::CompletionError(
            "error sending request for url: connection refused".to_string(),
        );
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::ProviderError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 502, "Connection errors should map to 502");
                assert!(
                    !message.contains("connection refused"),
                    "Should not expose provider error details, got: {}",
                    message
                );
                assert!(
                    message.contains("unavailable"),
                    "Should use generic message, got: {}",
                    message
                );
            }
            other => panic!("Expected ProviderError, got {:?}", other),
        }
    }

    // ============================================
    // External provider error mapping tests (is_external: true)
    // ============================================

    #[test]
    fn test_map_provider_error_external_400_becomes_invalid_params() {
        let error = inference_providers::CompletionError::HttpError {
            status_code: 400,
            message: "This model's maximum context length is 131072 tokens".to_string(),
            is_external: true,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::InvalidParams(msg) => {
                assert!(
                    msg.contains("context length"),
                    "Should preserve client-facing error message, got: {}",
                    msg
                );
            }
            other => panic!("Expected InvalidParams, got {:?}", other),
        }
    }

    #[test]
    fn test_map_provider_error_external_404_becomes_502() {
        let error = inference_providers::CompletionError::HttpError {
            status_code: 404,
            message: "Model not found on external provider".to_string(),
            is_external: true,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::ProviderError {
                status_code,
                message,
            } => {
                assert_eq!(
                    status_code, 502,
                    "External 404 should map to 502, not InvalidModel"
                );
                assert!(
                    !message.contains("external provider"),
                    "Should not expose provider details, got: {}",
                    message
                );
                assert!(
                    message.contains("unavailable"),
                    "Should use generic message, got: {}",
                    message
                );
            }
            other => panic!("Expected ProviderError, got {:?}", other),
        }
    }

    #[test]
    fn test_map_provider_error_external_429_still_rate_limited() {
        let error = inference_providers::CompletionError::HttpError {
            status_code: 429,
            message: "Rate limit exceeded".to_string(),
            is_external: true,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        assert!(
            matches!(result, ports::CompletionError::RateLimitExceeded(_)),
            "External 429 should still be RateLimitExceeded, got {:?}",
            result
        );
    }

    #[test]
    fn test_map_provider_error_external_500_becomes_502() {
        let error = inference_providers::CompletionError::HttpError {
            status_code: 500,
            message: "External provider internal error".to_string(),
            is_external: true,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::ProviderError { status_code, .. } => {
                assert_eq!(status_code, 502, "External 500 should map to 502");
            }
            other => panic!("Expected ProviderError, got {:?}", other),
        }
    }

    #[test]
    fn test_map_provider_error_client_media_becomes_invalid_params() {
        // The pool classifies media fetch/decode failures on the RAW upstream
        // body and carries the verdict as ClientMediaError, so the status
        // mapping is a simple variant match — independent of the (sanitized)
        // message content. Must be a non-retryable 400 with a GENERIC message
        // that never echoes the carried body (URL / internal paths). Using a
        // URL-bearing body here proves the status no longer depends on markers
        // that URL-redaction would strip.
        let error = inference_providers::CompletionError::ClientMediaError(
            "HTTP error 500: 404, message='Not Found', url='https://x/y.jpg': \
             cannot identify image file <_io.BytesIO>"
                .to_string(),
        );
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::InvalidParams(out) => {
                assert!(
                    !out.contains("BytesIO") && !out.contains("http") && !out.contains("y.jpg"),
                    "must not echo carried provider body, got: {}",
                    out
                );
            }
            other => panic!(
                "ClientMediaError should map to InvalidParams, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_map_provider_error_408_becomes_504() {
        let error = inference_providers::CompletionError::HttpError {
            status_code: 408,
            message: "Request timeout".to_string(),
            is_external: true,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::ProviderError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 504, "408 should map to 504 gateway timeout");
                assert!(
                    message.contains("timed out"),
                    "Should indicate timeout, got: {}",
                    message
                );
            }
            other => panic!("Expected ProviderError, got {:?}", other),
        }
    }

    #[test]
    fn test_map_provider_error_413_becomes_invalid_params() {
        let error = inference_providers::CompletionError::HttpError {
            status_code: 413,
            message: "Request body too large".to_string(),
            is_external: false,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::InvalidParams(msg) => {
                assert!(
                    msg.contains("too large"),
                    "Should preserve message, got: {}",
                    msg
                );
            }
            other => panic!("Expected InvalidParams, got {:?}", other),
        }
    }

    #[test]
    fn test_map_provider_error_422_becomes_invalid_params() {
        let error = inference_providers::CompletionError::HttpError {
            status_code: 422,
            message: "Invalid parameter: temperature must be between 0 and 2".to_string(),
            is_external: true,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::InvalidParams(msg) => {
                assert!(
                    msg.contains("temperature"),
                    "Should preserve message, got: {}",
                    msg
                );
            }
            other => panic!("Expected InvalidParams, got {:?}", other),
        }
    }

    #[test]
    fn test_map_provider_error_no_pubkey_provider_returns_421() {
        let error = inference_providers::CompletionError::NoPubKeyProvider(
            "No provider found for model test-model with public key '59e5d3f7...'".to_string(),
        );
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::ProviderError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 421, "Should return 421 for stale pubkey");
                assert!(
                    message.contains("encryption key"),
                    "Should mention encryption key, got: {}",
                    message
                );
            }
            other => panic!("Expected ProviderError with 421, got {:?}", other),
        }
    }

    #[test]
    fn test_map_provider_error_timeout_becomes_504() {
        let error = inference_providers::CompletionError::Timeout {
            operation: "chat_completion".to_string(),
            timeout_seconds: 600,
        };
        let result =
            CompletionServiceImpl::map_provider_error("test-model", &error, "test", Uuid::nil());
        match result {
            ports::CompletionError::ProviderError {
                status_code,
                message,
            } => {
                assert_eq!(
                    status_code, 504,
                    "Per-call timeout should surface as Gateway Timeout"
                );
                assert!(
                    message.to_lowercase().contains("timed out"),
                    "User-facing message should mention timeout, got: {}",
                    message
                );
            }
            other => panic!("Expected ProviderError with 504, got {:?}", other),
        }
    }

    fn chat_params_for_compat_tests(model: &str) -> inference_providers::ChatCompletionParams {
        inference_providers::ChatCompletionParams {
            model: model.to_string(),
            messages: vec![inference_providers::ChatMessage {
                role: inference_providers::MessageRole::User,
                content: Some(serde_json::json!("hi")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: None,
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            n: None,
            stream: Some(false),
            stop: None,
            frequency_penalty: None,
            presence_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: None,
            seed: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: None,
            store: None,
            stream_options: None,
            modalities: None,
            extra: std::collections::HashMap::new(),
        }
    }

    fn thinking_kwargs(params: &inference_providers::ChatCompletionParams) -> &serde_json::Value {
        params
            .extra
            .get("chat_template_kwargs")
            .expect("chat_template_kwargs should be present")
    }

    #[test]
    fn deepseek_compat_disables_thinking_for_json_object() {
        let mut params = chat_params_for_compat_tests(DEEPSEEK_V4_FLASH_MODEL);
        params.extra.insert(
            "response_format".to_string(),
            serde_json::json!({"type": "json_object"}),
        );
        params.extra.insert(
            "chat_template_kwargs".to_string(),
            serde_json::json!({"thinking": true, "enable_thinking": true}),
        );

        CompletionServiceImpl::apply_deepseek_v4_flash_thinking_compat(
            DEEPSEEK_V4_FLASH_MODEL,
            &mut params,
        );

        let kwargs = thinking_kwargs(&params);
        assert_eq!(kwargs["thinking"], serde_json::json!(false));
        assert_eq!(kwargs["enable_thinking"], serde_json::json!(false));
    }

    #[test]
    fn deepseek_compat_disables_thinking_for_forced_tool_choice() {
        let mut params = chat_params_for_compat_tests(DEEPSEEK_V4_FLASH_MODEL);
        params.tool_choice = Some(inference_providers::ToolChoice::Function {
            type_: "function".to_string(),
            function: inference_providers::FunctionChoice {
                name: "calculate".to_string(),
            },
        });
        params.extra.insert(
            "chat_template_kwargs".to_string(),
            serde_json::json!({"thinking": true, "enable_thinking": true}),
        );

        CompletionServiceImpl::apply_deepseek_v4_flash_thinking_compat(
            DEEPSEEK_V4_FLASH_MODEL,
            &mut params,
        );

        let kwargs = thinking_kwargs(&params);
        assert_eq!(kwargs["thinking"], serde_json::json!(false));
        assert_eq!(kwargs["enable_thinking"], serde_json::json!(false));
    }

    #[test]
    fn deepseek_compat_leaves_plain_reasoning_request_unchanged() {
        let mut params = chat_params_for_compat_tests(DEEPSEEK_V4_FLASH_MODEL);
        params
            .extra
            .insert("reasoning_effort".to_string(), serde_json::json!("high"));
        params.extra.insert(
            "chat_template_kwargs".to_string(),
            serde_json::json!({"thinking": true, "enable_thinking": true}),
        );

        CompletionServiceImpl::apply_deepseek_v4_flash_thinking_compat(
            DEEPSEEK_V4_FLASH_MODEL,
            &mut params,
        );

        let kwargs = thinking_kwargs(&params);
        assert_eq!(kwargs["thinking"], serde_json::json!(true));
        assert_eq!(kwargs["enable_thinking"], serde_json::json!(true));
    }

    #[test]
    fn deepseek_compat_is_model_gated() {
        let mut params = chat_params_for_compat_tests("Qwen/Qwen3.6-35B-A3B-FP8");
        params.extra.insert(
            "response_format".to_string(),
            serde_json::json!({"type": "json_object"}),
        );
        params.extra.insert(
            "chat_template_kwargs".to_string(),
            serde_json::json!({"thinking": true, "enable_thinking": true}),
        );

        CompletionServiceImpl::apply_deepseek_v4_flash_thinking_compat(
            "Qwen/Qwen3.6-35B-A3B-FP8",
            &mut params,
        );

        let kwargs = thinking_kwargs(&params);
        assert_eq!(kwargs["thinking"], serde_json::json!(true));
        assert_eq!(kwargs["enable_thinking"], serde_json::json!(true));
    }

    // ── extract_tools_from_extra ───────────────────────────────────

    #[test]
    fn extract_tools_consumes_function_shaped_tools() {
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "tools".to_string(),
            serde_json::json!([{
                "type": "function",
                "function": {
                    "name": "calc",
                    "description": "add",
                    "parameters": {"type": "object", "properties": {}}
                }
            }]),
        );
        extra.insert("tool_choice".to_string(), serde_json::json!("auto"));

        let (tools, tool_choice) = CompletionServiceImpl::extract_tools_from_extra(&mut extra);

        assert!(tools.is_some(), "function tool should parse");
        assert_eq!(tools.unwrap().len(), 1);
        assert!(tool_choice.is_some(), "tool_choice 'auto' should parse");
        // Successfully-parsed keys are consumed from extra so they aren't
        // also serialized via the flatten — would otherwise produce
        // duplicate keys in the upstream JSON.
        assert!(!extra.contains_key("tools"));
        assert!(!extra.contains_key("tool_choice"));
    }

    #[test]
    fn extract_tools_passes_through_unknown_tool_types() {
        // NEAR's `web_context_search` is a namespaced tool type handled
        // natively by the inference-proxy inside the CVM. It has no
        // `function` field, so it doesn't fit our typed `ToolDefinition`.
        // The helper must NOT silently drop it — leaving it in `extra`
        // lets it flow through to the upstream request body verbatim
        // via the flattened serialization.
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "tools".to_string(),
            serde_json::json!([{"type": "web_context_search"}]),
        );

        let (tools, tool_choice) = CompletionServiceImpl::extract_tools_from_extra(&mut extra);

        assert!(
            tools.is_none(),
            "non-function-shaped tools shouldn't bind the typed field"
        );
        assert!(tool_choice.is_none());
        // The raw value MUST still be in extra so it survives serialization
        // through to the upstream request body.
        let preserved = extra.get("tools").expect("tools must remain in extra");
        assert_eq!(
            preserved,
            &serde_json::json!([{"type": "web_context_search"}])
        );
    }

    #[test]
    fn extract_tools_passes_through_mixed_unknown_in_array() {
        // If one of the tools in the array can't be parsed as a function
        // tool, we conservatively leave the whole array in `extra` rather
        // than silently dropping some entries — cloud-api isn't the right
        // layer to decide which tools the upstream can handle.
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "tools".to_string(),
            serde_json::json!([
                {
                    "type": "function",
                    "function": {
                        "name": "calc",
                        "description": "add",
                        "parameters": {"type": "object", "properties": {}}
                    }
                },
                {"type": "web_context_search"}
            ]),
        );

        let (tools, _) = CompletionServiceImpl::extract_tools_from_extra(&mut extra);

        assert!(tools.is_none());
        assert!(extra.contains_key("tools"));
    }

    #[test]
    fn extract_tools_strips_function_tools_when_choice_none() {
        // nearai/cloud-api #619: tool_choice="none" must remove the tools so a
        // backend that ignores tool_choice (vLLM) cannot emit a tool call.
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "tools".to_string(),
            serde_json::json!([{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather.",
                    "parameters": {"type": "object", "properties": {}}
                }
            }]),
        );
        extra.insert("tool_choice".to_string(), serde_json::json!("none"));

        let (tools, tool_choice) = CompletionServiceImpl::extract_tools_from_extra(&mut extra);

        assert!(
            tools.is_none(),
            "tools must be stripped when choice is none"
        );
        assert!(
            matches!(tool_choice, Some(inference_providers::ToolChoice::String(ref s)) if s == "none"),
            "tool_choice=none should still be returned"
        );
        assert!(!extra.contains_key("tools"));
    }

    #[test]
    fn extract_tools_strips_unparsed_extra_tools_when_choice_none() {
        // Even tools that didn't parse into the typed field (and would normally
        // flow through via `extra`) must be removed when choice is "none".
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "tools".to_string(),
            serde_json::json!([{"type": "web_context_search"}]),
        );
        extra.insert("tool_choice".to_string(), serde_json::json!("none"));

        let (tools, _) = CompletionServiceImpl::extract_tools_from_extra(&mut extra);

        assert!(tools.is_none());
        assert!(
            !extra.contains_key("tools"),
            "tool_choice=none must also strip unparsed `tools` from extra"
        );
    }

    #[test]
    fn extract_tools_passes_through_unknown_tool_choice() {
        // Symmetric to tools: a non-standard tool_choice string survives
        // for upstream interpretation.
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "tool_choice".to_string(),
            serde_json::json!({"weird": "shape"}),
        );

        let (_, tool_choice) = CompletionServiceImpl::extract_tools_from_extra(&mut extra);

        assert!(tool_choice.is_none());
        assert!(extra.contains_key("tool_choice"));
    }

    #[test]
    fn extract_stream_options_consumes_typed_options() {
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "stream_options".to_string(),
            serde_json::json!({
                "include_usage": true,
                "continuous_usage_stats": false,
                "future_vendor_option": "preserved"
            }),
        );

        let stream_options = CompletionServiceImpl::extract_stream_options_from_extra(&mut extra)
            .expect("stream_options should parse");

        assert_eq!(stream_options.include_usage, Some(true));
        assert_eq!(stream_options.continuous_usage_stats, Some(false));
        assert_eq!(
            stream_options.extra.get("future_vendor_option"),
            Some(&serde_json::json!("preserved"))
        );
        assert!(
            !extra.contains_key("stream_options"),
            "typed stream_options should not also serialize through extra"
        );
    }

    #[test]
    fn extract_stream_options_passes_through_malformed_options() {
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "stream_options".to_string(),
            serde_json::json!("not-an-object"),
        );

        let stream_options = CompletionServiceImpl::extract_stream_options_from_extra(&mut extra);

        assert!(stream_options.is_none());
        assert!(
            extra.contains_key("stream_options"),
            "malformed stream_options should be left for upstream handling"
        );
    }

    // ── reject_n_gt_1_if_unsupported ──────────────────────────────────────

    #[test]
    fn reject_n_gt_1_external_provider_n_is_2() {
        // External providers (attestation_supported = false) must reject n > 1.
        let result = CompletionServiceImpl::reject_n_gt_1_if_unsupported(
            false,
            Some(2),
            "anthropic/claude-haiku-4-5",
        );
        assert!(result.is_err(), "n=2 on external provider must be rejected");
        match result.unwrap_err() {
            ports::CompletionError::InvalidParams(msg) => {
                assert!(
                    msg.contains("n>1"),
                    "Error message must mention n>1, got: {msg}"
                );
                assert!(
                    msg.contains("anthropic/claude-haiku-4-5"),
                    "Error message must include model name, got: {msg}"
                );
            }
            other => panic!("Expected InvalidParams, got {:?}", other),
        }
    }

    #[test]
    fn reject_n_gt_1_external_provider_n_is_1_allowed() {
        // n=1 is fine even for external providers.
        let result = CompletionServiceImpl::reject_n_gt_1_if_unsupported(
            false,
            Some(1),
            "anthropic/claude-haiku-4-5",
        );
        assert!(result.is_ok(), "n=1 on external provider must be allowed");
    }

    #[test]
    fn reject_n_gt_1_external_provider_n_none_allowed() {
        // n not set is fine even for external providers.
        let result = CompletionServiceImpl::reject_n_gt_1_if_unsupported(
            false,
            None,
            "anthropic/claude-haiku-4-5",
        );
        assert!(
            result.is_ok(),
            "n=None on external provider must be allowed"
        );
    }

    #[test]
    fn reject_n_gt_1_self_hosted_n_is_large_allowed() {
        // Self-hosted models (attestation_supported = true) support n > 1.
        let result = CompletionServiceImpl::reject_n_gt_1_if_unsupported(
            true,
            Some(5),
            "openai/gpt-oss-120b",
        );
        assert!(
            result.is_ok(),
            "n=5 on self-hosted model must be allowed, self-hosted supports n>1"
        );
    }
}
