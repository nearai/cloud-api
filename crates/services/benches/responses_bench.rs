//! Benchmarks for the Responses API streaming hot path.
//!
//! Covers ResponseStreamEvent serialization (delta and created variants),
//! SSE formatting, tokio::sync::Mutex vs std::sync::Mutex accumulation,
//! full 200-event stream simulation, and SHA-256 hashing at various sizes.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use sha2::{Digest, Sha256};

use services::responses::models::{
    ResponseContentItem, ResponseItemStatus, ResponseObject, ResponseOutputItem, ResponseStatus,
    ResponseStreamEvent, ResponseToolChoiceOutput, Usage,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a minimal delta event (the most frequent event type during streaming).
fn make_delta_event(seq: u64) -> ResponseStreamEvent {
    ResponseStreamEvent {
        event_type: "response.output_text.delta".to_string(),
        sequence_number: Some(seq),
        response: None,
        output_index: Some(0),
        content_index: Some(0),
        item: None,
        item_id: None,
        part: None,
        delta: Some("token".to_string()),
        text: None,
        logprobs: None,
        obfuscation: None,
        annotation_index: None,
        annotation: None,
        conversation_title: None,
    }
}

/// Build a response.created event containing a full ResponseObject.
fn make_created_event() -> ResponseStreamEvent {
    let response = ResponseObject {
        id: "resp_bench000000000000000000000001".to_string(),
        object: "response".to_string(),
        created_at: 1700000000,
        status: ResponseStatus::InProgress,
        background: false,
        conversation: None,
        error: None,
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        max_tool_calls: None,
        model: "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
        output: vec![ResponseOutputItem::Message {
            id: "msg_bench00000000000000000000001".to_string(),
            response_id: "resp_bench000000000000000000000001".to_string(),
            previous_response_id: None,
            next_response_ids: vec![],
            created_at: 1700000000,
            status: ResponseItemStatus::InProgress,
            role: "assistant".to_string(),
            content: vec![ResponseContentItem::OutputText {
                text: String::new(),
                annotations: vec![],
                logprobs: vec![],
            }],
            model: "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
            metadata: None,
        }],
        parallel_tool_calls: false,
        previous_response_id: None,
        next_response_ids: vec![],
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: None,
        safety_identifier: None,
        service_tier: "default".to_string(),
        store: false,
        temperature: 1.0,
        tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
        tools: vec![],
        top_logprobs: 0,
        top_p: 1.0,
        truncation: "disabled".to_string(),
        usage: Usage::new(0, 0),
        user: None,
        metadata: None,
    };

    ResponseStreamEvent {
        event_type: "response.created".to_string(),
        sequence_number: Some(0),
        response: Some(response),
        output_index: None,
        content_index: None,
        item: None,
        item_id: None,
        part: None,
        delta: None,
        text: None,
        logprobs: None,
        obfuscation: None,
        annotation_index: None,
        annotation: None,
        conversation_title: None,
    }
}

// ---------------------------------------------------------------------------
// Benchmark group: event_serialization
// ---------------------------------------------------------------------------

fn bench_event_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("response_event_serialize");

    let delta = make_delta_event(1);
    let created = make_created_event();

    group.bench_function("delta", |b| {
        b.iter(|| black_box(serde_json::to_string(black_box(&delta)).unwrap()))
    });

    group.bench_function("created", |b| {
        b.iter(|| black_box(serde_json::to_string(black_box(&created)).unwrap()))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: sse_formatting
// ---------------------------------------------------------------------------

fn bench_sse_formatting(c: &mut Criterion) {
    let mut group = c.benchmark_group("sse_format_response");

    let delta = make_delta_event(1);
    let json = serde_json::to_string(&delta).unwrap();
    let event_type = &delta.event_type;

    group.bench_function("format_sse_line", |b| {
        b.iter(|| {
            black_box(format!(
                "event: {}\ndata: {}\n\n",
                black_box(event_type),
                black_box(&json)
            ))
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: mutex_accumulation (tokio vs std)
// ---------------------------------------------------------------------------

fn bench_mutex_accumulation(c: &mut Criterion) {
    let mut group = c.benchmark_group("mutex_accumulation");

    let chunk = b"token_chunk_data_here_";

    // tokio::sync::Mutex
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mutex = tokio::sync::Mutex::new(Vec::<u8>::with_capacity(4096));

        group.bench_function("tokio_mutex_200_events", |b| {
            b.iter(|| {
                rt.block_on(async {
                    // Reset
                    mutex.lock().await.clear();
                    for _ in 0..200 {
                        let mut guard = mutex.lock().await;
                        guard.extend_from_slice(black_box(chunk));
                    }
                    black_box(mutex.lock().await.len());
                })
            })
        });
    }

    // std::sync::Mutex
    {
        let mutex = std::sync::Mutex::new(Vec::<u8>::with_capacity(4096));

        group.bench_function("std_mutex_200_events", |b| {
            b.iter(|| {
                mutex.lock().unwrap().clear();
                for _ in 0..200 {
                    let mut guard = mutex.lock().unwrap();
                    guard.extend_from_slice(black_box(chunk));
                }
                black_box(mutex.lock().unwrap().len());
            })
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: full_response_stream
// ---------------------------------------------------------------------------

fn bench_full_response_stream(c: &mut Criterion) {
    let mut group = c.benchmark_group("response_stream");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Pre-build 200 delta events.
    let events: Vec<ResponseStreamEvent> = (0..200).map(make_delta_event).collect();

    let accumulated = tokio::sync::Mutex::new(Vec::<u8>::with_capacity(32 * 1024));

    group.throughput(Throughput::Elements(200));

    group.bench_function("200_delta_events", |b| {
        b.iter(|| {
            rt.block_on(async {
                accumulated.lock().await.clear();
                for event in &events {
                    let json = serde_json::to_string(event).unwrap();
                    let sse = format!("event: {}\ndata: {}\n\n", event.event_type, json);
                    let mut guard = accumulated.lock().await;
                    guard.extend_from_slice(sse.as_bytes());
                }
                black_box(accumulated.lock().await.len());
            })
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: sha256_accumulated
// ---------------------------------------------------------------------------

fn bench_sha256_accumulated(c: &mut Criterion) {
    let mut group = c.benchmark_group("sha256_accumulated");

    for size_kb in [1, 10, 100] {
        let data = vec![b'x'; size_kb * 1024];

        group.throughput(Throughput::Bytes(data.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("hash", format!("{}kb", size_kb)),
            &data,
            |b, data| {
                b.iter(|| {
                    let mut hasher = Sha256::new();
                    hasher.update(black_box(data));
                    let hash = hasher.finalize();
                    black_box(hex::encode(hash))
                })
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_event_serialization,
    bench_sse_formatting,
    bench_mutex_accumulation,
    bench_full_response_stream,
    bench_sha256_accumulated,
);
criterion_main!(benches);
