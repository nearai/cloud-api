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
        let clock = FakeClock::new();
        let metrics = Arc::new(CapturingMetricsService::new());
        let cache = ApiKeySpendCache::new(ttl_secs, source.clone(), metrics.clone(), clock.clone());
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
    // 5% of 1000 = 50: remaining 50 is near, 51 is not.
    assert_eq!(required_max_age_ms(950, 1000, ttl).max_age_ms, 1_000);
    assert!(required_max_age_ms(950, 1000, ttl).near_limit);
    assert_eq!(required_max_age_ms(949, 1000, ttl).max_age_ms, ttl);
    // At or over the limit the short bound applies too.
    for spend in [1000, 1001, 5000] {
        let f = required_max_age_ms(spend, 1000, ttl);
        assert_eq!(f.max_age_ms, 1_000, "spend {spend}");
        assert!(f.near_limit, "spend {spend}");
    }
    // The short bound never exceeds the TTL.
    assert_eq!(required_max_age_ms(1000, 1000, 500).max_age_ms, 500);
    // Exactly at the 5% boundary (remaining 50 of 1000) and one unit either
    // side.
    assert!(required_max_age_ms(950, 1000, ttl).near_limit);
    assert!(required_max_age_ms(951, 1000, ttl).near_limit);
    assert!(!required_max_age_ms(949, 1000, ttl).near_limit);
    assert_eq!(required_max_age_ms(949, 1000, ttl).max_age_ms, ttl);
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
