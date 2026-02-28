//! Benchmarks for the authentication hot path.
//!
//! Covers API key format validation, SHA-256 hashing, Moka cache hit/miss,
//! bloom filter positive/negative checks, and the combined fast-path simulation.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use services::common::{hash_api_key, is_valid_api_key_format};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a realistic API key string (sk- prefix + 32 hex chars = 35 total).
fn make_api_key(seed: u64) -> String {
    format!("sk-{:032x}", seed)
}

// ---------------------------------------------------------------------------
// Benchmark group: api_key_validation
// ---------------------------------------------------------------------------

fn bench_api_key_validation(c: &mut Criterion) {
    let mut group = c.benchmark_group("api_key_validation");

    let valid_key = make_api_key(42);
    let invalid_key = "bad-key";

    group.bench_function("format_validation_valid", |b| {
        b.iter(|| black_box(is_valid_api_key_format(black_box(&valid_key))))
    });

    group.bench_function("format_validation_invalid", |b| {
        b.iter(|| black_box(is_valid_api_key_format(black_box(invalid_key))))
    });

    group.bench_function("sha256_hash", |b| {
        b.iter(|| black_box(hash_api_key(black_box(&valid_key))))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: moka_cache
// ---------------------------------------------------------------------------

fn bench_moka_cache(c: &mut Criterion) {
    let mut group = c.benchmark_group("api_key_cache");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Build a cache matching production parameters (10K cap, 30s TTL).
    let cache: moka::future::Cache<String, String> = moka::future::Cache::builder()
        .max_capacity(10_000)
        .time_to_live(std::time::Duration::from_secs(30))
        .build();

    let key = make_api_key(1);
    let hashed = hash_api_key(&key);

    // Pre-populate for hit benchmark.
    rt.block_on(cache.insert(hashed.clone(), "dummy-user-id".to_string()));

    group.bench_function("cache_hit", |b| {
        b.iter(|| rt.block_on(async { black_box(cache.get(black_box(&hashed)).await) }))
    });

    let missing_key = hash_api_key(&make_api_key(999_999));

    group.bench_function("cache_miss", |b| {
        b.iter(|| rt.block_on(async { black_box(cache.get(black_box(&missing_key)).await) }))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: bloom_filter
// ---------------------------------------------------------------------------

fn bench_bloom_filter(c: &mut Criterion) {
    let mut group = c.benchmark_group("bloom_filter");

    // Populate bloom filter with 1000 hashed keys.
    let mut bloom = bloomfilter::Bloom::new_for_fp_rate(1000, 0.01).unwrap();
    let mut known_hash = String::new();
    for i in 0..1000u64 {
        let h = hash_api_key(&make_api_key(i));
        if i == 500 {
            known_hash = h.clone();
        }
        bloom.set(&h);
    }

    let absent_hash = hash_api_key(&make_api_key(1_000_000));

    group.bench_function("check_positive", |b| {
        b.iter(|| black_box(bloom.check(black_box(&known_hash))))
    });

    group.bench_function("check_negative", |b| {
        b.iter(|| black_box(bloom.check(black_box(&absent_hash))))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: full_auth_hot_path
// ---------------------------------------------------------------------------

fn bench_full_auth_hot_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("full_auth_hot_path");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let cache: moka::future::Cache<String, String> = moka::future::Cache::builder()
        .max_capacity(10_000)
        .time_to_live(std::time::Duration::from_secs(30))
        .build();

    let key = make_api_key(42);
    let hashed = hash_api_key(&key);
    rt.block_on(cache.insert(hashed.clone(), "user-id-abc".to_string()));

    // Simulate: format check → SHA-256 → cache hit.
    group.bench_function("format_hash_cache_hit", |b| {
        b.iter(|| {
            let k = black_box(&key);
            assert!(is_valid_api_key_format(k));
            let h = hash_api_key(k);
            rt.block_on(async { black_box(cache.get(&h).await) })
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_api_key_validation,
    bench_moka_cache,
    bench_bloom_filter,
    bench_full_auth_hot_path,
);
criterion_main!(benches);
