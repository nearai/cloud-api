use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::ports::{ApiKeySpendSource, Clock};
use super::*;
use crate::metrics::capturing::{CapturingMetricsService, MetricValue};

pub(super) const T0: u64 = 1_700_000_000_000;

pub(super) struct FakeClock(AtomicU64);

impl FakeClock {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(T0)))
    }
    pub(super) fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Counts reads and can hold every read until released.
pub(super) struct FakeSource {
    spend: AtomicI64,
    reads: AtomicUsize,
    gate: Option<Arc<Semaphore>>,
    pub(super) fail: Mutex<bool>,
    /// Runs inside every read, before the value is returned (a usage row
    /// committing while the database sum runs).
    pub(super) hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl FakeSource {
    pub(super) fn new(spend: i64) -> Arc<Self> {
        Arc::new(Self {
            spend: AtomicI64::new(spend),
            reads: AtomicUsize::new(0),
            gate: None,
            fail: Mutex::new(false),
            hook: Mutex::new(None),
        })
    }
    pub(super) fn gated(spend: i64, gate: Arc<Semaphore>) -> Arc<Self> {
        Arc::new(Self {
            spend: AtomicI64::new(spend),
            reads: AtomicUsize::new(0),
            gate: Some(gate),
            fail: Mutex::new(false),
            hook: Mutex::new(None),
        })
    }
    pub(super) fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
    pub(super) fn set(&self, spend: i64) {
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
        if let Some(hook) = self.hook.lock().unwrap().as_ref() {
            hook();
        }
        if *self.fail.lock().unwrap() {
            anyhow::bail!("database down");
        }
        Ok(self.spend.load(Ordering::SeqCst))
    }
}

pub(super) struct Harness {
    pub(super) cache: ApiKeySpendCache,
    pub(super) source: Arc<FakeSource>,
    pub(super) clock: Arc<FakeClock>,
    pub(super) metrics: Arc<CapturingMetricsService>,
    pub(super) id: Uuid,
    pub(super) shared: Option<Arc<SharedTotal>>,
}

impl Harness {
    pub(super) fn new(source: Arc<FakeSource>, ttl_secs: u64) -> Self {
        let clock = FakeClock::new();
        let metrics = Arc::new(CapturingMetricsService::new());
        let cache = ApiKeySpendCache::new(ttl_secs, source.clone(), metrics.clone(), clock.clone());
        Self {
            cache,
            source,
            clock,
            metrics,
            id: Uuid::new_v4(),
            shared: None,
        }
    }

    /// A cache with the shared total over `store`.
    pub(super) fn with_total(
        source: Arc<FakeSource>,
        ttl_secs: u64,
        store: Arc<super::fake_store::FakeStore>,
        mode: TotalMode,
    ) -> Self {
        let clock = FakeClock::new();
        let metrics = Arc::new(CapturingMetricsService::new());
        let shared = SharedTotal::new(store, clock.clone(), metrics.clone(), mode);
        let cache = ApiKeySpendCache::new_with_total(
            ttl_secs,
            source.clone(),
            metrics.clone(),
            clock.clone(),
            Some(shared.clone()),
        );
        Self {
            cache,
            source,
            clock,
            metrics,
            id: Uuid::new_v4(),
            shared: Some(shared),
        }
    }

    /// Number of counter events named `name`, optionally with a tag.
    pub(super) fn count(&self, name: &str, tag: Option<&str>) -> i64 {
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

pub(super) const LIMIT: i64 = 1_000_000;

// --- the freshness rule ---

#[test]
fn freshness_rule_boundaries() {
    let ttl = 10_000;
    // 5% of 1000 = 50: remaining 50 is near, 51 is not.
    assert_eq!(required_max_age_ms(950, 1000, ttl).max_age_ms, 1_000);
    assert!(required_max_age_ms(950, 1000, ttl).near_limit);
    assert_eq!(required_max_age_ms(949, 1000, ttl).max_age_ms, ttl);
    // At or over the limit a reading may reject for the whole TTL.
    assert_eq!(required_max_age_ms(1000, 1000, ttl).max_age_ms, ttl);
    assert_eq!(required_max_age_ms(5000, 1000, ttl).max_age_ms, ttl);
    // A zero limit has no margin and is always over.
    assert_eq!(required_max_age_ms(0, 0, ttl).max_age_ms, ttl);
    // A huge limit does not overflow.
    assert_eq!(required_max_age_ms(0, i64::MAX, ttl).max_age_ms, ttl);
    assert!(required_max_age_ms(i64::MAX - 1, i64::MAX, ttl).near_limit);
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
async fn over_limit_entry_rejects_from_cache_within_the_ttl() {
    let h = Harness::new(FakeSource::new(LIMIT), 10);
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), LIMIT);
    h.clock.advance(9_000);
    // 9 s old, over the limit: still served, no near-limit refresh.
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), LIMIT);
    assert_eq!(h.source.reads(), 1);
    h.clock.advance(1_500);
    h.cache.api_key_spend(h.id, LIMIT).await.unwrap();
    assert_eq!(h.source.reads(), 2);
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
