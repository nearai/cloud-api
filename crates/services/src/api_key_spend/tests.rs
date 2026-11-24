use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::ports::{ApiKeySpendSource, Clock};
use super::*;
use crate::metrics::capturing::{CapturingMetricsService, MetricValue};

const T0: u64 = 1_700_000_000_000;

struct FakeClock(AtomicU64);

impl FakeClock {
    fn new() -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(T0)))
    }
    fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Counts reads and can hold every read until released.
struct FakeSource {
    spend: AtomicI64,
    reads: AtomicUsize,
    gate: Option<Arc<Semaphore>>,
    fail: Mutex<bool>,
}

impl FakeSource {
    fn new(spend: i64) -> Arc<Self> {
        Arc::new(Self {
            spend: AtomicI64::new(spend),
            reads: AtomicUsize::new(0),
            gate: None,
            fail: Mutex::new(false),
        })
    }
    fn gated(spend: i64, gate: Arc<Semaphore>) -> Arc<Self> {
        Arc::new(Self {
            spend: AtomicI64::new(spend),
            reads: AtomicUsize::new(0),
            gate: Some(gate),
            fail: Mutex::new(false),
        })
    }
    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
    fn set(&self, spend: i64) {
        self.spend.store(spend, Ordering::SeqCst);
    }
}

#[async_trait]
impl ApiKeySpendSource for FakeSource {
    async fn api_key_spend(&self, _id: Uuid) -> anyhow::Result<i64> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        if *self.fail.lock().unwrap() {
            anyhow::bail!("database down");
        }
        Ok(self.spend.load(Ordering::SeqCst))
    }
}

struct Harness {
    cache: ApiKeySpendCache,
    source: Arc<FakeSource>,
    clock: Arc<FakeClock>,
    metrics: Arc<CapturingMetricsService>,
    id: Uuid,
}

impl Harness {
    fn new(source: Arc<FakeSource>, ttl_secs: u64) -> Self {
        Self::with_read_timeout(source, ttl_secs, Duration::from_secs(30))
    }

    fn with_read_timeout(source: Arc<FakeSource>, ttl_secs: u64, read_timeout: Duration) -> Self {
        let clock = FakeClock::new();
        let metrics = Arc::new(CapturingMetricsService::new());
        let cache = ApiKeySpendCache::new(
            ttl_secs,
            read_timeout,
            source.clone(),
            metrics.clone(),
            clock.clone(),
        );
        Self {
            cache,
            source,
            clock,
            metrics,
            id: Uuid::new_v4(),
        }
    }

    /// Number of counter events named `name`, optionally with a tag.
    fn count(&self, name: &str, tag: Option<&str>) -> i64 {
        self.metrics
            .get_metrics()
            .iter()
            .filter(|m| m.name == name)
            .filter(|m| tag.is_none_or(|t| m.tags.iter().any(|x| x == t)))
            .map(|m| match m.value {
                MetricValue::Count(n) => n,
                _ => 0,
            })
            .sum()
    }
}

const LIMIT: i64 = 1_000_000;

// --- the freshness rule ---

#[test]
fn freshness_rule_boundaries() {
    let ttl = 10_000;
    // 5% of 1000 = 50: remaining 50 (spend 950) is near, remaining 51 (spend
    // 949) is not.
    assert_eq!(required_max_age_ms(950, 1000, ttl).max_age_ms, 1_000);
    assert!(required_max_age_ms(950, 1000, ttl).near_limit);
    assert_eq!(required_max_age_ms(949, 1000, ttl).max_age_ms, ttl);
    assert!(!required_max_age_ms(949, 1000, ttl).near_limit);
    assert!(required_max_age_ms(951, 1000, ttl).near_limit);
    // At or over the limit the short bound applies too.
    for spend in [1000, 1001, 5000] {
        let f = required_max_age_ms(spend, 1000, ttl);
        assert_eq!(f.max_age_ms, 1_000, "spend {spend}");
        assert!(f.near_limit, "spend {spend}");
    }
    // The short bound never exceeds the TTL.
    assert_eq!(required_max_age_ms(1000, 1000, 500).max_age_ms, 500);
    // A zero or negative limit has no margin and is always at or over.
    assert_eq!(required_max_age_ms(0, 0, ttl).max_age_ms, 1_000);
    assert_eq!(required_max_age_ms(0, -5, ttl).max_age_ms, 1_000);
    assert_eq!(required_max_age_ms(-10, -5, ttl).max_age_ms, ttl);
    assert_eq!(
        required_max_age_ms(i64::MIN, i64::MIN, ttl).max_age_ms,
        1_000
    );
    // A huge limit does not overflow, and the margin stays 5%.
    assert_eq!(required_max_age_ms(0, i64::MAX, ttl).max_age_ms, ttl);
    assert!(required_max_age_ms(i64::MAX - 1, i64::MAX, ttl).near_limit);
    let huge_96_percent = i64::MAX / 100 * 96;
    assert!(required_max_age_ms(huge_96_percent, i64::MAX, ttl).near_limit);
    let huge_94_percent = i64::MAX / 100 * 94;
    assert!(!required_max_age_ms(huge_94_percent, i64::MAX, ttl).near_limit);
}

// --- the clock ---

#[test]
fn production_clock_never_goes_backwards_and_entry_age_follows_it() {
    // The wall clock can step back; a monotonic source cannot, so a backwards
    // step is not expressible here. This proves the production clock is
    // non-decreasing across calls (including after a real delay) and that an
    // entry's age is the plain difference of two of its readings.
    let clock = ports::SystemClock;
    let mut last = clock.now_ms();
    for _ in 0..1_000 {
        let now = clock.now_ms();
        assert!(now >= last, "clock went backwards: {last} -> {now}");
        last = now;
    }
    let read_at_ms = clock.now_ms();
    std::thread::sleep(Duration::from_millis(30));
    let later = clock.now_ms();
    assert!(later >= read_at_ms + 30);
    let entry = Entry {
        spend: 1,
        read_at_ms,
    };
    assert_eq!(entry.age_ms(later), later - read_at_ms);
    assert_eq!(entry.age_ms(read_at_ms), 0);
}

// --- L1 ---

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_misses_run_one_database_read() {
    let gate = Arc::new(Semaphore::new(0));
    let h = Harness::new(FakeSource::gated(100, gate.clone()), 10);
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let cache = h.cache.clone();
        let id = h.id;
        tasks.push(tokio::spawn(async move {
            cache.api_key_spend(id, LIMIT).await.unwrap()
        }));
    }
    // The first load is blocked inside the source; give every other caller
    // time to reach the cache. Without coalescing they would each be inside
    // the source by now.
    while h.source.reads() == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.source.reads(), 1, "misses must share one load");
    gate.add_permits(100);
    for t in tasks {
        assert_eq!(t.await.unwrap(), 100);
    }
    assert_eq!(h.source.reads(), 1);
}

#[tokio::test]
async fn second_call_is_an_l1_hit_until_the_ttl_passes() {
    let h = Harness::new(FakeSource::new(100), 10);
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 100);
    h.source.set(200);
    h.clock.advance(9_999);
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 100);
    assert_eq!(h.source.reads(), 1);
    assert_eq!(h.count(METRIC_API_KEY_SPEND_L1_HIT, None), 1);
    // Past the TTL the entry is read again and the new value is seen.
    h.clock.advance(2);
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 200);
    assert_eq!(h.source.reads(), 2);
}

#[tokio::test]
async fn near_limit_entry_older_than_a_second_forces_a_fresh_read() {
    let h = Harness::new(FakeSource::new(960_000), 10);
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 960_000);
    // Within one second the near-limit entry is still used.
    h.clock.advance(900);
    h.source.set(990_000);
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 960_000);
    assert_eq!(h.source.reads(), 1);
    // At 5 s it is well inside the TTL but too old for a near-limit key.
    h.clock.advance(4_100);
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 990_000);
    assert_eq!(h.source.reads(), 2);
    assert_eq!(h.count(METRIC_API_KEY_SPEND_NEAR_LIMIT_REFRESH, None), 1);
}

#[tokio::test]
async fn entry_far_from_the_limit_is_not_refreshed_early() {
    let h = Harness::new(FakeSource::new(100), 10);
    h.cache.api_key_spend(h.id, LIMIT).await.unwrap();
    h.clock.advance(5_000);
    h.cache.api_key_spend(h.id, LIMIT).await.unwrap();
    assert_eq!(h.source.reads(), 1);
    assert_eq!(h.count(METRIC_API_KEY_SPEND_NEAR_LIMIT_REFRESH, None), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_near_limit_refreshes_share_one_read() {
    let gate = Arc::new(Semaphore::new(1));
    let h = Harness::new(FakeSource::gated(960_000, gate.clone()), 10);
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 960_000);
    h.clock.advance(5_000);
    h.source.set(995_000);
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let cache = h.cache.clone();
        let id = h.id;
        tasks.push(tokio::spawn(async move {
            cache.api_key_spend(id, LIMIT).await.unwrap()
        }));
    }
    while h.source.reads() < 2 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        h.source.reads(),
        2,
        "one initial read plus one shared refresh"
    );
    gate.add_permits(100);
    for t in tasks {
        assert_eq!(t.await.unwrap(), 995_000);
    }
    assert_eq!(h.source.reads(), 2);
}

#[tokio::test]
async fn over_limit_entry_is_reread_after_a_second_and_sees_a_lower_spend() {
    let h = Harness::new(FakeSource::new(LIMIT + 10), 10);
    assert_eq!(
        h.cache.api_key_spend(h.id, LIMIT).await.unwrap(),
        LIMIT + 10
    );
    // Within one second the over-limit reading is reused: no second read.
    h.clock.advance(900);
    h.source.set(LIMIT - 500_000);
    assert_eq!(
        h.cache.api_key_spend(h.id, LIMIT).await.unwrap(),
        LIMIT + 10
    );
    assert_eq!(h.source.reads(), 1);
    // After one second it is read again and the lower spend is returned, so
    // the caller stops rejecting.
    h.clock.advance(200);
    assert_eq!(
        h.cache.api_key_spend(h.id, LIMIT).await.unwrap(),
        LIMIT - 500_000
    );
    assert_eq!(h.source.reads(), 2);
    assert_eq!(h.count(METRIC_API_KEY_SPEND_NEAR_LIMIT_REFRESH, None), 1);
}

#[tokio::test]
async fn ttl_zero_reads_the_database_every_time() {
    let h = Harness::new(FakeSource::new(5), 0);
    for _ in 0..3 {
        assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 5);
    }
    assert_eq!(h.source.reads(), 3);
}

#[tokio::test]
async fn database_error_is_returned_and_not_cached() {
    let h = Harness::new(FakeSource::new(5), 10);
    *h.source.fail.lock().unwrap() = true;
    assert!(h.cache.api_key_spend(h.id, LIMIT).await.is_err());
    *h.source.fail.lock().unwrap() = false;
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 5);
    assert_eq!(h.source.reads(), 2);
}

// --- slow reads ---

/// Starts a check while the source holds its read, advances the fake clock by
/// `read_ms` (the read "takes" that long), then lets the read finish.
async fn call_with_slow_read(h: &Harness, gate: &Arc<Semaphore>, read_ms: u64) -> i64 {
    let cache = h.cache.clone();
    let id = h.id;
    let before = h.source.reads();
    let task = tokio::spawn(async move { cache.api_key_spend(id, LIMIT).await.unwrap() });
    while h.source.reads() == before {
        tokio::task::yield_now().await;
    }
    h.clock.advance(read_ms);
    gate.add_permits(1);
    task.await.unwrap()
}

#[tokio::test]
async fn a_read_slower_than_the_bound_is_used_without_a_second_read() {
    for first in [960_000, LIMIT + 10] {
        let gate = Arc::new(Semaphore::new(0));
        let h = Harness::new(FakeSource::gated(first, gate.clone()), 10);
        // The read takes 4 s against the 1 s near-limit bound: the reading is
        // already older than the bound when it arrives, and is returned as is.
        assert_eq!(call_with_slow_read(&h, &gate, 4_000).await, first);
        assert_eq!(h.source.reads(), 1, "first {first}");
        // The next call finds that reading too old and reads once more, not
        // twice.
        assert_eq!(call_with_slow_read(&h, &gate, 4_000).await, first);
        assert_eq!(h.source.reads(), 2, "first {first}");
    }
}

// --- read timeout ---

/// A source whose read never completes.
struct HungSource {
    reads: AtomicUsize,
    hang: Mutex<bool>,
}

#[async_trait]
impl ApiKeySpendSource for HungSource {
    async fn api_key_spend(&self, _id: Uuid) -> anyhow::Result<i64> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if *self.hang.lock().unwrap() {
            std::future::pending::<()>().await;
        }
        Ok(7)
    }
}

fn hung_cache(
    ttl_secs: u64,
    timeout: Duration,
) -> (
    ApiKeySpendCache,
    Arc<HungSource>,
    Arc<CapturingMetricsService>,
) {
    let source = Arc::new(HungSource {
        reads: AtomicUsize::new(0),
        hang: Mutex::new(true),
    });
    let metrics = Arc::new(CapturingMetricsService::new());
    let cache = ApiKeySpendCache::new(
        ttl_secs,
        timeout,
        source.clone(),
        metrics.clone(),
        FakeClock::new(),
    );
    (cache, source, metrics)
}

fn timeout_count(metrics: &CapturingMetricsService) -> i64 {
    metrics
        .get_metrics()
        .iter()
        .filter(|m| m.name == METRIC_API_KEY_SPEND_DB_READ_TIMEOUT)
        .map(|m| match m.value {
            MetricValue::Count(n) => n,
            _ => 0,
        })
        .sum()
}

#[tokio::test(start_paused = true)]
async fn a_read_that_never_completes_fails_after_the_timeout_and_is_not_cached() {
    let (cache, source, metrics) = hung_cache(10, Duration::from_secs(30));
    let id = Uuid::new_v4();
    let started = tokio::time::Instant::now();
    assert!(cache.api_key_spend(id, LIMIT).await.is_err());
    assert_eq!(started.elapsed(), Duration::from_secs(30));
    assert_eq!(source.reads.load(Ordering::SeqCst), 1);
    assert_eq!(timeout_count(&metrics), 1);
    // The failure is not cached: the next call reads again and succeeds.
    *source.hang.lock().unwrap() = false;
    assert_eq!(cache.api_key_spend(id, LIMIT).await.unwrap(), 7);
    assert_eq!(source.reads.load(Ordering::SeqCst), 2);
    assert_eq!(timeout_count(&metrics), 1);
}

#[tokio::test(start_paused = true)]
async fn the_timeout_applies_with_the_cache_disabled() {
    let (cache, source, metrics) = hung_cache(0, Duration::from_secs(5));
    assert!(cache.api_key_spend(Uuid::new_v4(), LIMIT).await.is_err());
    assert_eq!(source.reads.load(Ordering::SeqCst), 1);
    assert_eq!(timeout_count(&metrics), 1);
}

/// A source that finishes after a fixed delay.
struct DelayedSource(Duration);

#[async_trait]
impl ApiKeySpendSource for DelayedSource {
    async fn api_key_spend(&self, _id: Uuid) -> anyhow::Result<i64> {
        tokio::time::sleep(self.0).await;
        Ok(11)
    }
}

#[tokio::test(start_paused = true)]
async fn a_read_that_finishes_just_inside_the_timeout_succeeds() {
    let metrics = Arc::new(CapturingMetricsService::new());
    let cache = ApiKeySpendCache::new(
        10,
        Duration::from_secs(30),
        Arc::new(DelayedSource(Duration::from_millis(29_999))),
        metrics.clone(),
        FakeClock::new(),
    );
    assert_eq!(
        cache.api_key_spend(Uuid::new_v4(), LIMIT).await.unwrap(),
        11
    );
    assert_eq!(timeout_count(&metrics), 0);
}

// --- overshoot ---

/// Steady spend rate with requests arriving every 100 ms: how long the cache
/// keeps returning an under-limit reading, and so how far past the limit the
/// source is when the cache first reports it.
async fn first_over_limit_report(first_reading: i64, rate_per_sec: i64) -> (u64, i64) {
    const LIMIT: i64 = 100;
    let h = Harness::new(FakeSource::new(first_reading), 10);
    assert_eq!(
        h.cache.api_key_spend(h.id, LIMIT).await.unwrap(),
        first_reading
    );
    let mut elapsed_ms = 0;
    loop {
        h.clock.advance(100);
        elapsed_ms += 100;
        h.source
            .set(first_reading + rate_per_sec * elapsed_ms as i64 / 1_000);
        let seen = h.cache.api_key_spend(h.id, LIMIT).await.unwrap();
        if seen >= LIMIT {
            return (elapsed_ms, h.source.spend.load(Ordering::SeqCst) - LIMIT);
        }
        assert!(elapsed_ms < 30_000, "never reported");
    }
}

#[tokio::test]
async fn overshoot_is_a_full_ttl_of_spend_far_from_the_limit_and_a_second_inside_the_band() {
    // Reading 90 of 100 is more than 5% below the limit, so it is reused for
    // the whole 10 s TTL (the first read after it is at 10.1 s). At 20 per
    // second the source is at 90 + 20 * 10.1 = 292 when the cache first
    // reports the key over its limit: 192 past it.
    let (ms, over) = first_over_limit_report(90, 20).await;
    assert_eq!(ms, 10_100);
    assert_eq!(over, 192);

    // Reading 96 is inside the 5% band, so it is reused for 1 s only. The
    // first fresh read (at 1.1 s) sees 96 + 22 = 118: 18 past the limit.
    let (ms, over) = first_over_limit_report(96, 20).await;
    assert_eq!(ms, 1_100);
    assert_eq!(over, 18);
}
