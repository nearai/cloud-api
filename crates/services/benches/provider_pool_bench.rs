//! Benchmarks for the inference provider pool hot path.
//!
//! Covers round-robin index key formatting, mutex-guarded selection, provider
//! ordering with varying pool sizes, sticky routing cache, RwLock+HashMap model
//! lookup, and pub-key filtering via `Arc::ptr_eq`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use async_trait::async_trait;
use inference_providers::models::{
    AttestationError, AudioTranscriptionError, AudioTranscriptionParams,
    AudioTranscriptionResponse, ChatCompletionParams, ChatCompletionResponseWithBytes,
    ChatSignature, CompletionError, CompletionParams, ImageEditError, ImageEditParams,
    ImageEditResponseWithBytes, ImageGenerationError, ImageGenerationParams,
    ImageGenerationResponseWithBytes, ListModelsError, ModelsResponse, RerankError, RerankParams,
    RerankResponse, ScoreError, ScoreParams, ScoreResponse,
};
use inference_providers::{InferenceProvider, StreamingResult};

// ---------------------------------------------------------------------------
// Stub provider (all methods unimplemented — we never call them)
// ---------------------------------------------------------------------------

struct StubProvider;

#[async_trait]
impl InferenceProvider for StubProvider {
    async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        unimplemented!()
    }
    async fn chat_completion_stream(
        &self,
        _: ChatCompletionParams,
        _: String,
    ) -> Result<StreamingResult, CompletionError> {
        unimplemented!()
    }
    async fn chat_completion(
        &self,
        _: ChatCompletionParams,
        _: String,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        unimplemented!()
    }
    async fn text_completion_stream(
        &self,
        _: CompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        unimplemented!()
    }
    async fn image_generation(
        &self,
        _: ImageGenerationParams,
        _: String,
    ) -> Result<ImageGenerationResponseWithBytes, ImageGenerationError> {
        unimplemented!()
    }
    async fn image_edit(
        &self,
        _: Arc<ImageEditParams>,
        _: String,
    ) -> Result<ImageEditResponseWithBytes, ImageEditError> {
        unimplemented!()
    }
    async fn score(&self, _: ScoreParams, _: String) -> Result<ScoreResponse, ScoreError> {
        unimplemented!()
    }
    async fn rerank(&self, _: RerankParams) -> Result<RerankResponse, RerankError> {
        unimplemented!()
    }
    async fn get_signature(
        &self,
        _: &str,
        _: Option<String>,
    ) -> Result<ChatSignature, CompletionError> {
        unimplemented!()
    }
    async fn get_attestation_report(
        &self,
        _: String,
        _: Option<String>,
        _: Option<String>,
        _: Option<String>,
    ) -> Result<serde_json::Map<String, serde_json::Value>, AttestationError> {
        unimplemented!()
    }
    async fn audio_transcription(
        &self,
        _: AudioTranscriptionParams,
        _: String,
    ) -> Result<AudioTranscriptionResponse, AudioTranscriptionError> {
        unimplemented!()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type DynProvider = Arc<dyn InferenceProvider + Send + Sync>;

fn make_providers(n: usize) -> Vec<DynProvider> {
    (0..n)
        .map(|_| Arc::new(StubProvider) as DynProvider)
        .collect()
}

// ---------------------------------------------------------------------------
// Benchmark group: round_robin
// ---------------------------------------------------------------------------

fn bench_round_robin(c: &mut Criterion) {
    let mut group = c.benchmark_group("round_robin");

    let model_id = "Qwen/Qwen3-30B-A3B-Instruct-2507";

    group.bench_function("index_key_format", |b| {
        b.iter(|| black_box(format!("id:{}", black_box(model_id))))
    });

    // Simulate the mutex-guarded round-robin selection (matches production code).
    let index: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let providers = make_providers(3);
    let key = format!("id:{}", model_id);

    group.bench_function("mutex_and_select_3", |b| {
        b.iter(|| {
            let mut guard = index.lock().unwrap();
            let entry = guard.entry(key.clone()).or_insert(0);
            let selected = *entry % providers.len();
            *entry = (*entry + 1) % providers.len();
            black_box(selected);
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: provider_ordering
// ---------------------------------------------------------------------------

fn bench_provider_ordering(c: &mut Criterion) {
    let mut group = c.benchmark_group("provider_ordering");

    let index: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

    for n in [3, 10] {
        let providers = make_providers(n);
        let key = format!("id:model-{}", n);

        group.bench_with_input(BenchmarkId::new("order_providers", n), &n, |b, _| {
            b.iter(|| {
                let mut guard = index.lock().unwrap();
                let entry = guard.entry(key.clone()).or_insert(0);
                let start = *entry % providers.len();
                *entry = (*entry + 1) % providers.len();

                // Build ordered vec rotating from start index (matches production).
                let mut ordered = Vec::with_capacity(providers.len());
                for i in 0..providers.len() {
                    ordered.push(providers[(start + i) % providers.len()].clone());
                }
                black_box(ordered);
            })
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: sticky_routing
// ---------------------------------------------------------------------------

fn bench_sticky_routing(c: &mut Criterion) {
    let mut group = c.benchmark_group("sticky_routing");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Production parameters: 100K cap, 1h TTL.
    let cache: moka::future::Cache<String, DynProvider> = moka::future::Cache::builder()
        .max_capacity(100_000)
        .time_to_live(Duration::from_secs(3600))
        .build();

    let chat_id = "chatcmpl-abc123def456".to_string();
    let provider: DynProvider = Arc::new(StubProvider);

    rt.block_on(cache.insert(chat_id.clone(), provider));

    group.bench_function("cache_hit", |b| {
        b.iter(|| rt.block_on(async { black_box(cache.get(black_box(&chat_id)).await) }))
    });

    let missing_chat_id = "chatcmpl-missing-999".to_string();

    group.bench_function("cache_miss", |b| {
        b.iter(|| rt.block_on(async { black_box(cache.get(black_box(&missing_chat_id)).await) }))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: rwlock_model_lookup
// ---------------------------------------------------------------------------

fn bench_rwlock_model_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("rwlock_model_lookup");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut model_map: HashMap<String, Vec<DynProvider>> = HashMap::new();
    model_map.insert(
        "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
        make_providers(3),
    );
    model_map.insert("meta-llama/Llama-3-70B".to_string(), make_providers(5));

    let lock = Arc::new(tokio::sync::RwLock::new(model_map));
    let model_id = "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string();

    group.bench_function("read_and_lookup", |b| {
        b.iter(|| {
            rt.block_on(async {
                let guard = lock.read().await;
                black_box(guard.get(black_box(&model_id)).is_some())
            })
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: pubkey_filtering
// ---------------------------------------------------------------------------

fn bench_pubkey_filtering(c: &mut Criterion) {
    let mut group = c.benchmark_group("pubkey_filtering");

    // Simulate N model providers and M pubkey providers, intersect via Arc::ptr_eq.
    let all_providers = make_providers(5);

    // Model has all 5 providers.
    let model_providers = all_providers.clone();
    // Pubkey matches providers 1 and 3 (simulate 2 out of 5).
    let pubkey_providers = [all_providers[1].clone(), all_providers[3].clone()];

    group.bench_function("ptr_eq_5_providers", |b| {
        b.iter(|| {
            let intersection: Vec<_> = model_providers
                .iter()
                .filter(|mp| pubkey_providers.iter().any(|pp| Arc::ptr_eq(mp, pp)))
                .cloned()
                .collect();
            black_box(intersection);
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_round_robin,
    bench_provider_ordering,
    bench_sticky_routing,
    bench_rwlock_model_lookup,
    bench_pubkey_filtering,
);
criterion_main!(benches);
