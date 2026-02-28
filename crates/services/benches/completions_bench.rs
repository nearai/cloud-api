//! Criterion microbenchmarks for completions hot-path optimizations.
//!
//! Three benchmark groups:
//! 1. **sse_token_processing** — per-token `.map()` closure (old async vs new sync path)
//! 2. **intercept_stream** — `InterceptStream::poll_next` throughput over 200 tokens
//! 3. **model_resolution_cache** — moka cache hit vs miss for `resolve_and_get_model`

use bytes::Bytes;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use futures::stream::StreamExt;
use std::sync::Arc;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Helpers: build synthetic SSE events
// ---------------------------------------------------------------------------

/// Build a realistic SSE `data: {...}\n` payload for a chat completion chunk.
fn make_sse_payload(index: usize, is_last: bool) -> Bytes {
    let usage = if is_last {
        r#","usage":{"prompt_tokens":50,"completion_tokens":200,"total_tokens":250}"#
    } else {
        ""
    };
    let finish_reason = if is_last { r#""stop""# } else { "null" };
    let content = if is_last {
        String::new()
    } else {
        format!("token_{index}")
    };

    let json = format!(
        r#"data: {{"id":"chatcmpl-bench000","object":"chat.completion.chunk","created":1700000000,"model":"bench-model","choices":[{{"index":0,"delta":{{"content":"{content}"}},"finish_reason":{finish_reason}}}]{usage}}}"#,
    );
    Bytes::from(format!("{json}\n"))
}

/// Build a Vec of raw SSE byte payloads simulating a 200-token stream.
fn make_sse_payloads(n: usize) -> Vec<Bytes> {
    (0..n).map(|i| make_sse_payload(i, i == n - 1)).collect()
}

/// Build SSEEvent objects for InterceptStream benchmarks.
fn make_sse_events(
    n: usize,
) -> Vec<Result<inference_providers::SSEEvent, inference_providers::CompletionError>> {
    (0..n)
        .map(|i| {
            let is_last = i == n - 1;
            let raw_bytes = make_sse_payload(i, is_last);

            let content = if is_last {
                String::new()
            } else {
                format!("token_{i}")
            };
            let finish_reason = if is_last {
                Some(inference_providers::FinishReason::Stop)
            } else {
                None
            };
            let usage = if is_last {
                Some(inference_providers::TokenUsage::new(50, 200))
            } else {
                None
            };

            let chunk = inference_providers::models::ChatCompletionChunk {
                id: "chatcmpl-bench000".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1_700_000_000,
                model: "bench-model".to_string(),
                system_fingerprint: None,
                choices: vec![inference_providers::models::ChatChoice {
                    index: 0,
                    delta: Some(inference_providers::ChatDelta {
                        role: None,
                        content: Some(content),
                        name: None,
                        tool_call_id: None,
                        tool_calls: None,
                        reasoning_content: None,
                        reasoning: None,
                    }),
                    logprobs: None,
                    finish_reason,
                    token_ids: None,
                }],
                usage,
                prompt_token_ids: None,
                modality: None,
            };

            Ok(inference_providers::SSEEvent {
                raw_bytes,
                chunk: inference_providers::StreamChunk::Chat(chunk),
            })
        })
        .collect()
}

// ===========================================================================
// Group 1: SSE token processing — old (async) vs new (sync) per-token path
// ===========================================================================

/// **Old path** (pre-optimisation):
/// - `tokio::Mutex` for accumulated_bytes and chat_id_state
/// - `String::from_utf8` (allocating) on every token
/// - Parse JSON on every token to extract chat_id
/// - Uses `.then()` (async) on the stream
async fn process_stream_old(payloads: Vec<Bytes>) {
    let accumulated_bytes = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
    let chat_id_state = Arc::new(tokio::sync::Mutex::new(None::<String>));

    let stream = futures::stream::iter(payloads.into_iter().map(Ok::<_, std::convert::Infallible>));

    let _: Vec<_> = stream
        .then(move |result| {
            let accumulated = accumulated_bytes.clone();
            let chat_id = chat_id_state.clone();
            async move {
                let event_bytes = result.unwrap();

                // Parse JSON on every token (old behaviour)
                if let Ok(chunk_str) = String::from_utf8(event_bytes.to_vec()) {
                    if let Some(data) = chunk_str.strip_prefix("data: ") {
                        if let Ok(serde_json::Value::Object(obj)) =
                            serde_json::from_str::<serde_json::Value>(data.trim())
                        {
                            if let Some(serde_json::Value::String(id)) = obj.get("id") {
                                let mut guard = chat_id.lock().await;
                                *guard = Some(id.clone());
                            }
                        }
                    }
                }

                // Accumulate bytes
                let raw_str = String::from_utf8_lossy(&event_bytes);
                let json_data = raw_str
                    .trim()
                    .strip_prefix("data: ")
                    .unwrap_or(raw_str.trim());
                let sse_bytes = Bytes::from(format!("data: {json_data}\n\n"));
                accumulated.lock().await.extend_from_slice(&sse_bytes);
                sse_bytes
            }
        })
        .collect()
        .await;
}

/// **New path** (optimised):
/// - `std::sync::Mutex` for accumulated_bytes and chat_id_state
/// - `std::str::from_utf8` (zero-copy) for chat_id extraction
/// - Parse JSON only on first token (skip when chat_id already set)
/// - Uses `.map()` (sync) on the stream
/// Build and drive the new-path stream. The caller provides the runtime to avoid
/// measuring runtime-construction overhead inside `b.iter()`.
fn process_stream_new(payloads: Vec<Bytes>, rt: &tokio::runtime::Runtime) {
    let accumulated_bytes = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let chat_id_state = Arc::new(std::sync::Mutex::new(None::<String>));

    let stream = futures::stream::iter(payloads.into_iter().map(Ok::<_, std::convert::Infallible>));

    let accumulated_clone = accumulated_bytes.clone();
    let chat_id_clone = chat_id_state.clone();

    let mapped = stream.map(move |result| {
        let event_bytes = result.unwrap();

        // Only parse JSON for chat_id on first token
        {
            let mut guard = chat_id_clone.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_none() {
                if let Ok(chunk_str) = std::str::from_utf8(&event_bytes) {
                    if let Some(data) = chunk_str.strip_prefix("data: ") {
                        if let Ok(serde_json::Value::Object(obj)) =
                            serde_json::from_str::<serde_json::Value>(data.trim())
                        {
                            if let Some(serde_json::Value::String(id)) = obj.get("id") {
                                *guard = Some(id.clone());
                            }
                        }
                    }
                }
            }
        }

        // Accumulate bytes
        let raw_str = String::from_utf8_lossy(&event_bytes);
        let json_data = raw_str
            .trim()
            .strip_prefix("data: ")
            .unwrap_or(raw_str.trim());
        let sse_bytes = Bytes::from(format!("data: {json_data}\n\n"));
        accumulated_clone
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(&sse_bytes);
        sse_bytes
    });

    rt.block_on(async {
        let _: Vec<_> = mapped.collect().await;
    });
}

fn bench_sse_token_processing(c: &mut Criterion) {
    let mut group = c.benchmark_group("sse_token_processing");
    let token_count: usize = 200;
    group.throughput(Throughput::Elements(token_count as u64));

    let payloads = make_sse_payloads(token_count);

    group.bench_with_input(
        BenchmarkId::new("old_async_path", token_count),
        &payloads,
        |b, payloads| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            b.iter(|| {
                rt.block_on(process_stream_old(payloads.clone()));
            });
        },
    );

    group.bench_with_input(
        BenchmarkId::new("new_sync_path", token_count),
        &payloads,
        |b, payloads| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            b.iter(|| {
                process_stream_new(payloads.clone(), &rt);
            });
        },
    );

    group.finish();
}

// ===========================================================================
// Group 2: InterceptStream poll_next throughput
// ===========================================================================

// Stub implementations for services (no-op / instant return)

struct NoOpMetrics;

#[async_trait::async_trait]
impl services::metrics::MetricsServiceTrait for NoOpMetrics {
    fn record_latency(&self, _name: &str, _duration: std::time::Duration, _tags: &[&str]) {}
    fn record_count(&self, _name: &str, _value: i64, _tags: &[&str]) {}
    fn record_histogram(&self, _name: &str, _value: f64, _tags: &[&str]) {}
}

struct NoOpAttestation;

#[async_trait::async_trait]
impl services::attestation::ports::AttestationServiceTrait for NoOpAttestation {
    async fn get_chat_signature(
        &self,
        _chat_id: &str,
        _signing_algo: Option<String>,
    ) -> Result<
        services::attestation::models::SignatureLookupResult,
        services::attestation::models::AttestationError,
    > {
        unimplemented!("not used in benchmark")
    }
    async fn store_chat_signature_from_provider(
        &self,
        _chat_id: &str,
    ) -> Result<(), services::attestation::models::AttestationError> {
        Ok(())
    }
    async fn store_response_signature(
        &self,
        _response_id: &str,
        _request_hash: String,
        _response_hash: String,
    ) -> Result<(), services::attestation::models::AttestationError> {
        Ok(())
    }
    async fn get_attestation_report(
        &self,
        _model: Option<String>,
        _signing_algo: Option<String>,
        _nonce: Option<String>,
        _signing_address: Option<String>,
    ) -> Result<
        services::attestation::models::AttestationReport,
        services::attestation::models::AttestationError,
    > {
        unimplemented!("not used in benchmark")
    }
    async fn verify_vpc_signature(
        &self,
        _timestamp: i64,
        _signature: String,
    ) -> Result<bool, services::attestation::models::AttestationError> {
        unimplemented!("not used in benchmark")
    }
}

struct NoOpUsage;

#[async_trait::async_trait]
impl services::usage::UsageServiceTrait for NoOpUsage {
    async fn calculate_cost(
        &self,
        _model_id: &str,
        _input_tokens: i32,
        _output_tokens: i32,
    ) -> Result<services::usage::CostBreakdown, services::usage::UsageError> {
        unimplemented!("not used in benchmark")
    }
    async fn record_usage(
        &self,
        _request: services::usage::RecordUsageServiceRequest,
    ) -> Result<services::usage::UsageLogEntry, services::usage::UsageError> {
        Ok(dummy_usage_log_entry())
    }
    async fn record_usage_from_api(
        &self,
        _organization_id: uuid::Uuid,
        _workspace_id: uuid::Uuid,
        _api_key_id: uuid::Uuid,
        _request: services::usage::RecordUsageApiRequest,
    ) -> Result<services::usage::UsageLogEntry, services::usage::UsageError> {
        unimplemented!("not used in benchmark")
    }
    async fn check_can_use(
        &self,
        _organization_id: uuid::Uuid,
    ) -> Result<services::usage::UsageCheckResult, services::usage::UsageError> {
        unimplemented!("not used in benchmark")
    }
    async fn get_balance(
        &self,
        _organization_id: uuid::Uuid,
    ) -> Result<Option<services::usage::OrganizationBalanceInfo>, services::usage::UsageError> {
        unimplemented!("not used in benchmark")
    }
    async fn get_usage_history(
        &self,
        _organization_id: uuid::Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> Result<(Vec<services::usage::UsageLogEntry>, i64), services::usage::UsageError> {
        unimplemented!("not used in benchmark")
    }
    async fn get_limit(
        &self,
        _organization_id: uuid::Uuid,
    ) -> Result<Option<services::usage::OrganizationLimit>, services::usage::UsageError> {
        unimplemented!("not used in benchmark")
    }
    async fn get_usage_history_by_api_key(
        &self,
        _api_key_id: uuid::Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> Result<(Vec<services::usage::UsageLogEntry>, i64), services::usage::UsageError> {
        unimplemented!("not used in benchmark")
    }
    async fn get_api_key_usage_history_with_permissions(
        &self,
        _workspace_id: uuid::Uuid,
        _api_key_id: uuid::Uuid,
        _user_id: uuid::Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> Result<(Vec<services::usage::UsageLogEntry>, i64), services::usage::UsageError> {
        unimplemented!("not used in benchmark")
    }
    async fn get_costs_by_inference_ids(
        &self,
        _organization_id: uuid::Uuid,
        _inference_ids: Vec<uuid::Uuid>,
    ) -> Result<Vec<services::usage::InferenceCost>, services::usage::UsageError> {
        unimplemented!("not used in benchmark")
    }
}

fn dummy_usage_log_entry() -> services::usage::UsageLogEntry {
    services::usage::UsageLogEntry {
        id: uuid::Uuid::nil(),
        organization_id: uuid::Uuid::nil(),
        workspace_id: uuid::Uuid::nil(),
        api_key_id: uuid::Uuid::nil(),
        model_id: uuid::Uuid::nil(),
        model: "bench-model".to_string(),
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        input_cost: 0,
        output_cost: 0,
        total_cost: 0,
        inference_type: services::usage::InferenceType::ChatCompletionStream,
        created_at: chrono::Utc::now(),
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: None,
        provider_request_id: None,
        stop_reason: None,
        response_id: None,
        image_count: None,
        was_inserted: true,
    }
}

fn build_intercept_stream(
    events: Vec<Result<inference_providers::SSEEvent, inference_providers::CompletionError>>,
) -> services::completions::InterceptStream<
    futures::stream::Iter<
        std::vec::IntoIter<
            Result<inference_providers::SSEEvent, inference_providers::CompletionError>,
        >,
    >,
> {
    let now = Instant::now();
    services::completions::InterceptStream {
        inner: futures::stream::iter(events),
        attestation_service: Arc::new(NoOpAttestation),
        usage_service: Arc::new(NoOpUsage),
        metrics_service: Arc::new(NoOpMetrics),
        organization_id: uuid::Uuid::nil(),
        workspace_id: uuid::Uuid::nil(),
        api_key_id: uuid::Uuid::nil(),
        model_id: uuid::Uuid::nil(),
        model_name: "bench-model".to_string(),
        inference_type: services::usage::InferenceType::ChatCompletionStream,
        service_start_time: now,
        provider_start_time: now,
        first_token_received: false,
        first_token_time: None,
        ttft_ms: None,
        token_count: 0,
        last_token_time: None,
        total_itl_ms: 0.0,
        metric_tags: vec![
            "model:bench-model".to_string(),
            "environment:bench".to_string(),
        ],
        concurrent_counter: None,
        last_usage_stats: None,
        last_chat_id: None,
        stream_completed: false,
        response_id: None,
        last_finish_reason: None,
        last_error: None,
        state: services::completions::StreamState::Streaming,
        attestation_supported: false,
    }
}

fn bench_intercept_stream(c: &mut Criterion) {
    let mut group = c.benchmark_group("intercept_stream");
    let token_count: usize = 200;
    group.throughput(Throughput::Elements(token_count as u64));

    let events = make_sse_events(token_count);

    group.bench_with_input(
        BenchmarkId::new("poll_next_200_tokens", token_count),
        &events,
        |b, events| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            b.iter(|| {
                let stream = build_intercept_stream(events.clone());
                rt.block_on(async {
                    let _: Vec<_> = stream.collect().await;
                });
            });
        },
    );

    group.finish();
}

// ===========================================================================
// Group 3: Model resolution cache — hit vs miss
// ===========================================================================

fn make_test_model() -> services::models::ModelWithPricing {
    services::models::ModelWithPricing {
        id: uuid::Uuid::nil(),
        model_name: "bench/test-model".to_string(),
        model_display_name: "Bench Test Model".to_string(),
        model_description: "A model for benchmarking".to_string(),
        model_icon: None,
        input_cost_per_token: 100,
        output_cost_per_token: 300,
        cost_per_image: 0,
        context_length: 8192,
        verifiable: false,
        aliases: vec!["bench-alias".to_string()],
        owned_by: "benchmark".to_string(),
        provider_type: "vllm".to_string(),
        provider_config: None,
        attestation_supported: false,
        input_modalities: Some(vec!["text".to_string()]),
        output_modalities: Some(vec!["text".to_string()]),
    }
}

fn bench_model_resolution_cache(c: &mut Criterion) {
    let mut group = c.benchmark_group("model_resolution_cache");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Build a moka cache matching production configuration (60s TTL, 1000 capacity)
    let cache: moka::future::Cache<String, Option<services::models::ModelWithPricing>> =
        moka::future::Cache::builder()
            .max_capacity(1_000)
            .time_to_live(std::time::Duration::from_secs(60))
            .build();

    // Pre-populate for cache-hit benchmark
    let model = make_test_model();
    rt.block_on(cache.insert("bench/test-model".to_string(), Some(model.clone())));

    group.bench_function("cache_hit", |b| {
        b.iter(|| {
            rt.block_on(async {
                let _ = cache.get("bench/test-model").await;
            });
        });
    });

    group.bench_function("cache_miss_and_insert", |b| {
        let model_for_insert = make_test_model();
        // Use iter_batched to create a fresh cache per iteration, guaranteeing a true miss.
        b.iter_batched(
            || {
                moka::future::Cache::builder()
                    .max_capacity(1_000)
                    .time_to_live(std::time::Duration::from_secs(60))
                    .build()
            },
            |miss_cache: moka::future::Cache<
                String,
                Option<services::models::ModelWithPricing>,
            >| {
                rt.block_on(async {
                    let key = "bench/test-model";
                    let cached = miss_cache.get(key).await;
                    if cached.is_none() {
                        miss_cache
                            .insert(key.to_string(), Some(model_for_insert.clone()))
                            .await;
                    }
                });
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ===========================================================================
// Criterion harness
// ===========================================================================

criterion_group!(
    benches,
    bench_sse_token_processing,
    bench_intercept_stream,
    bench_model_resolution_cache,
);
criterion_main!(benches);
