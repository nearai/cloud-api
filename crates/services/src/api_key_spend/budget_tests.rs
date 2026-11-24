use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::budget::{BudgetSettings, MIN_REFRESH_INTERVAL, OVER_LIMIT_RECHECK};
use super::ports::ApiKeySpendSource;
use super::tests::{FakeClock, T0};
use super::*;
use crate::metrics::capturing::{CapturingMetricsService, MetricValue};
use crate::metrics::consts::*;

const LIMIT: i64 = 1_000_000;
/// 10% of the headroom of an empty key.
const SLICE: i64 = 100_000;
/// A reading inside the 5% band (limit / 20 = 50,000): 55,000 of headroom, so
/// a slice of 5,500, and using it leaves 49,500.
const NEAR_SPEND: i64 = LIMIT - 55_000;
const NEAR_SLICE: i64 = 5_500;

/// Counts reads; can hold them, fail them or never finish them.
struct Source {
    spend: AtomicI64,
    reads: AtomicUsize,
    gate: Option<Arc<Semaphore>>,
    fail: AtomicBool,
    hang: AtomicBool,
    panic: AtomicBool,
}

impl Source {
    fn new(spend: i64) -> Arc<Self> {
        Self::build(spend, None)
    }
    fn gated(spend: i64, gate: Arc<Semaphore>) -> Arc<Self> {
        Self::build(spend, Some(gate))
    }
    fn build(spend: i64, gate: Option<Arc<Semaphore>>) -> Arc<Self> {
        Arc::new(Self {
            spend: AtomicI64::new(spend),
            reads: AtomicUsize::new(0),
            gate,
            fail: AtomicBool::new(false),
            hang: AtomicBool::new(false),
            panic: AtomicBool::new(false),
        })
    }
    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
    fn set(&self, spend: i64) {
        self.spend.store(spend, Ordering::SeqCst);
    }
    fn fail(&self, fail: bool) {
        self.fail.store(fail, Ordering::SeqCst);
    }
    fn panic(&self, panic: bool) {
        self.panic.store(panic, Ordering::SeqCst);
    }
    fn hang(&self, hang: bool) {
        self.hang.store(hang, Ordering::SeqCst);
    }
}

#[async_trait]
impl ApiKeySpendSource for Source {
    async fn api_key_spend(&self, _id: Uuid) -> anyhow::Result<i64> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.acquire().await?.forget();
        }
        if self.panic.load(Ordering::SeqCst) {
            panic!("source panicked");
        }
        if self.hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("database down");
        }
        Ok(self.spend.load(Ordering::SeqCst))
    }
}

struct Harness {
    cache: ApiKeySpendCache,
    source: Arc<Source>,
    clock: Arc<FakeClock>,
    metrics: Arc<CapturingMetricsService>,
    id: Uuid,
}

impl Harness {
    fn new(source: Arc<Source>) -> Self {
        Self::with_timeout(source, Duration::from_secs(30))
    }

    fn with_timeout(source: Arc<Source>, read_timeout: Duration) -> Self {
        Self::with_settings(source, read_timeout, 60)
    }

    fn with_settings(source: Arc<Source>, read_timeout: Duration, max_age_secs: u64) -> Self {
        let clock = FakeClock::new();
        let metrics = Arc::new(CapturingMetricsService::new());
        let cache = ApiKeySpendCache::new_with_budget(
            10,
            read_timeout,
            source.clone(),
            metrics.clone(),
            clock.clone(),
            Some(BudgetSettings {
                share_percent: 10,
                max_age_secs,
            }),
        );
        Self {
            cache,
            source,
            clock,
            metrics,
            id: Uuid::new_v4(),
        }
    }

    async fn check(&self) -> Decision {
        self.cache.check(self.id, LIMIT).await.unwrap()
    }

    async fn record(&self, cost: i64) {
        self.cache.record_cost(self.id, cost).await;
    }

    async fn snapshot(&self) -> Option<(i64, u64, i64)> {
        self.cache.budget_snapshot(self.id).await
    }

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

/// Lets spawned tasks run until they block.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

fn retry_after(decision: Decision) -> u64 {
    match decision {
        Decision::Throttle { retry_after_secs } => retry_after_secs,
        other => panic!("expected a throttle, got {other:?}"),
    }
}

// --- far from the limit ---

#[tokio::test(start_paused = true)]
async fn far_from_the_limit_a_thousand_checks_make_one_read() {
    let h = Harness::new(Source::new(0));
    for _ in 0..1_000 {
        assert_eq!(h.check().await, Decision::Admit);
        h.record(40).await;
    }
    settle().await;
    // 1,000 * 40 = 40,000, inside half a slice (50,000).
    assert_eq!(h.snapshot().await.unwrap().2, 40_000);
    assert_eq!(h.source.reads(), 1);
    assert_eq!(h.count(METRIC_API_KEY_SPEND_BUDGET_ADMIT, None), 1_000);
    assert_eq!(h.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, None), 0);
}

// --- crossing half a slice ---

#[tokio::test(start_paused = true)]
async fn crossing_half_a_slice_starts_one_refresh_that_admits_meanwhile() {
    let gate = Arc::new(Semaphore::new(1));
    let h = Harness::new(Source::gated(0, gate.clone()));
    assert_eq!(h.check().await, Decision::Admit);
    assert_eq!(h.snapshot().await.unwrap(), (0, T0, 0));

    h.clock.advance(6_000);
    h.record(SLICE / 2).await;
    assert_eq!(h.check().await, Decision::Admit);
    settle().await;
    assert_eq!(h.source.reads(), 2, "the refresh is blocked in the source");

    // The refresh is running: requests are admitted, none starts another.
    for _ in 0..50 {
        assert_eq!(h.check().await, Decision::Admit);
    }
    // A cost recorded after the refresh's read started counts for the new
    // anchor; one recorded before it does not.
    h.record(7).await;
    assert_eq!(h.snapshot().await.unwrap(), (0, T0, SLICE / 2 + 7));
    h.source.set(SLICE / 2);
    gate.add_permits(1);
    settle().await;

    assert_eq!(h.source.reads(), 2);
    assert_eq!(h.snapshot().await.unwrap(), (SLICE / 2, T0 + 6_000, 7));
    assert_eq!(
        h.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, Some("result:ok")),
        1
    );
    assert!(h
        .metrics
        .get_metrics()
        .iter()
        .any(|m| m.name == METRIC_API_KEY_SPEND_BUDGET_REFRESH_MS));
}

// --- slice used up ---

#[tokio::test(start_paused = true)]
async fn near_the_limit_a_used_up_slice_throttles_until_a_refresh_brings_headroom() {
    let gate = Arc::new(Semaphore::new(1));
    let h = Harness::new(Source::gated(NEAR_SPEND, gate.clone()));
    assert_eq!(h.check().await, Decision::Admit);
    h.record(NEAR_SLICE).await;

    let first = h.check().await;
    assert!(retry_after(first) >= 1);
    settle().await;
    assert_eq!(h.source.reads(), 2);
    // Still running: throttled, and no second read starts.
    for _ in 0..20 {
        assert!(retry_after(h.check().await) >= 1);
    }
    assert_eq!(h.source.reads(), 2);
    assert_eq!(h.count(METRIC_API_KEY_SPEND_BUDGET_THROTTLE, None), 21);

    // The database shows plenty of headroom after all.
    h.source.set(0);
    gate.add_permits(1);
    settle().await;
    assert_eq!(h.check().await, Decision::Admit);
    assert_eq!(h.snapshot().await.unwrap().0, 0);
    assert_eq!(h.snapshot().await.unwrap().2, 0);
}

#[tokio::test(start_paused = true)]
async fn far_from_the_limit_a_used_up_slice_admits_while_one_refresh_runs() {
    let gate = Arc::new(Semaphore::new(1));
    let h = Harness::new(Source::gated(0, gate.clone()));
    assert_eq!(h.check().await, Decision::Admit);
    h.record(SLICE).await;
    assert_eq!(h.check().await, Decision::Admit);
    settle().await;
    assert_eq!(h.source.reads(), 2, "one refresh, blocked in the source");
    for _ in 0..3 {
        h.record(SLICE).await;
        assert_eq!(h.check().await, Decision::Admit);
    }
    settle().await;
    assert_eq!(h.source.reads(), 2, "no second refresh while one runs");
    assert_eq!(h.count(METRIC_API_KEY_SPEND_BUDGET_THROTTLE, None), 0);
    h.source.set(4 * SLICE);
    gate.add_permits(1);
    settle().await;
    assert_eq!(h.snapshot().await.unwrap().0, 4 * SLICE);
    assert_eq!(h.check().await, Decision::Admit);
}

#[tokio::test(start_paused = true)]
async fn far_from_the_limit_failing_refreshes_admit_until_the_anchor_is_stale() {
    let h = Harness::new(Source::new(0));
    assert_eq!(h.check().await, Decision::Admit);
    h.record(SLICE).await;
    h.source.fail(true);
    // The max age is 60 s: up to 120 s the key is admitted past its slice,
    // with a refresh attempted (and failing) every 5 s.
    for _ in 0..24 {
        assert_eq!(h.check().await, Decision::Admit);
        settle().await;
        h.clock.advance(5_000);
    }
    assert!(h.source.reads() > 20);
    assert_eq!(h.snapshot().await.unwrap().1, T0);
    // 120 s exactly is still not stale; one more millisecond is.
    assert_eq!(h.check().await, Decision::Admit);
    h.clock.advance(1);
    assert!(retry_after(h.check().await) >= 1);
    settle().await;
    // A successful refresh admits again.
    h.source.fail(false);
    h.source.set(SLICE);
    h.clock.advance(5_000);
    assert!(retry_after(h.check().await) >= 1);
    settle().await;
    assert_eq!(h.check().await, Decision::Admit);
    assert_eq!(h.snapshot().await.unwrap().0, SLICE);
}

#[tokio::test(start_paused = true)]
async fn the_near_limit_band_boundary_is_inclusive() {
    // Anchor 940,000: 60,000 of headroom, slice 6,000, band 50,000. The
    // gate keeps the refreshes the checks start from finishing.
    let gate = Arc::new(Semaphore::new(1));
    let h = Harness::new(Source::gated(LIMIT - 60_000, gate));
    assert_eq!(h.check().await, Decision::Admit);
    // 10,000 left would be 50,000: 9,999 used leaves 50,001, just outside.
    h.record(9_999).await;
    assert_eq!(h.check().await, Decision::Admit);
    h.record(1).await;
    assert!(retry_after(h.check().await) >= 1);
}

#[tokio::test(start_paused = true)]
async fn the_near_limit_arithmetic_does_not_overflow() {
    // Huge limit, everything recorded: nothing left, so throttled.
    let h = Harness::new(Source::new(0));
    assert_eq!(
        h.cache.check(h.id, i64::MAX).await.unwrap(),
        Decision::Admit
    );
    h.record(i64::MAX).await;
    h.record(i64::MAX).await;
    assert!(retry_after(h.cache.check(h.id, i64::MAX).await.unwrap()) >= 1);
    // A negative anchor with a limit of zero, and spend beyond what is left.
    let h = Harness::new(Source::new(-5));
    assert_eq!(h.cache.check(h.id, 0).await.unwrap(), Decision::Admit);
    h.record(1_000).await;
    assert!(retry_after(h.cache.check(h.id, 0).await.unwrap()) >= 1);
    // The lowest anchor under the highest limit is far from it.
    let h = Harness::new(Source::new(i64::MIN));
    assert_eq!(
        h.cache.check(h.id, i64::MAX).await.unwrap(),
        Decision::Admit
    );
    h.record(i64::MAX / 5).await;
    assert_eq!(
        h.cache.check(h.id, i64::MAX).await.unwrap(),
        Decision::Admit
    );
}

#[tokio::test(start_paused = true)]
async fn retry_after_waits_out_the_refresh_interval_after_a_failed_refresh() {
    let h = Harness::new(Source::new(NEAR_SPEND));
    assert_eq!(h.check().await, Decision::Admit);
    h.record(NEAR_SLICE).await;
    h.source.fail(true);
    // The throttling check starts a refresh, which fails.
    assert!(retry_after(h.check().await) >= 1);
    settle().await;
    h.clock.advance(1_500);
    // 3.5 s are left of the 5 s interval: whole seconds, rounded up.
    assert_eq!(retry_after(h.check().await), 4);
    h.clock.advance(3_499);
    assert_eq!(retry_after(h.check().await), 1);
}

// --- age ---

#[tokio::test(start_paused = true)]
async fn an_anchor_older_than_the_max_age_admits_and_refreshes_once() {
    let gate = Arc::new(Semaphore::new(1));
    let h = Harness::new(Source::gated(0, gate.clone()));
    assert_eq!(h.check().await, Decision::Admit);
    h.clock.advance(60_000);
    assert_eq!(
        h.check().await,
        Decision::Admit,
        "exactly max age: no refresh"
    );
    settle().await;
    assert_eq!(h.source.reads(), 1);
    h.clock.advance(1);
    for _ in 0..10 {
        assert_eq!(h.check().await, Decision::Admit);
    }
    settle().await;
    assert_eq!(h.source.reads(), 2);
    h.source.set(500);
    gate.add_permits(1);
    settle().await;
    assert_eq!(h.snapshot().await.unwrap(), (500, T0 + 60_001, 0));
}

// --- over the limit ---

#[tokio::test(start_paused = true)]
async fn over_the_limit_rechecks_in_the_background_every_thirty_seconds() {
    let h = Harness::new(Source::new(LIMIT + 10));
    assert_eq!(h.check().await, Decision::OverLimit { spend: LIMIT + 10 });
    h.source.set(LIMIT - 500_000);
    let recheck = OVER_LIMIT_RECHECK.as_millis() as u64;
    h.clock.advance(recheck - 1);
    for _ in 0..10 {
        assert_eq!(h.check().await, Decision::OverLimit { spend: LIMIT + 10 });
    }
    settle().await;
    assert_eq!(h.source.reads(), 1, "no read for 30 s");
    h.clock.advance(1);
    // The request that finds the anchor old is still rejected; the read runs
    // behind it.
    assert_eq!(h.check().await, Decision::OverLimit { spend: LIMIT + 10 });
    settle().await;
    assert_eq!(h.source.reads(), 2);
    assert_eq!(h.check().await, Decision::Admit);
    assert_eq!(h.count(METRIC_API_KEY_SPEND_BUDGET_OVER_LIMIT, None), 12);
}

// --- failed refreshes ---

#[tokio::test(start_paused = true)]
async fn a_failed_refresh_keeps_the_anchor_and_waits_the_interval() {
    let h = Harness::new(Source::new(0));
    assert_eq!(h.check().await, Decision::Admit);
    let before = h.snapshot().await.unwrap();
    h.record(SLICE / 2).await;
    h.source.fail(true);
    assert_eq!(h.check().await, Decision::Admit);
    settle().await;
    assert_eq!(h.source.reads(), 2);
    assert_eq!(
        h.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, Some("result:error")),
        1
    );
    let after = h.snapshot().await.unwrap();
    assert_eq!((after.0, after.1), (before.0, before.1));
    assert_eq!(after.2, SLICE / 2);

    // The next attempt waits for the interval, however many requests come.
    let interval = MIN_REFRESH_INTERVAL.as_millis() as u64;
    h.clock.advance(interval - 1);
    for _ in 0..10 {
        assert_eq!(h.check().await, Decision::Admit);
    }
    settle().await;
    assert_eq!(h.source.reads(), 2);
    h.clock.advance(1);
    assert_eq!(h.check().await, Decision::Admit);
    settle().await;
    assert_eq!(h.source.reads(), 3);
    assert_eq!(
        h.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, Some("result:error")),
        2
    );
    // Recovery: the next successful refresh replaces the anchor.
    h.source.fail(false);
    h.source.set(SLICE / 2);
    h.clock.advance(interval);
    assert_eq!(h.check().await, Decision::Admit);
    settle().await;
    assert_eq!(h.snapshot().await.unwrap().0, SLICE / 2);
}

#[tokio::test(start_paused = true)]
async fn a_refresh_that_times_out_is_counted_and_never_an_error() {
    let h = Harness::with_timeout(Source::new(0), Duration::from_secs(2));
    assert_eq!(h.check().await, Decision::Admit);
    h.record(SLICE / 2).await;
    h.source.hang(true);
    assert_eq!(h.check().await, Decision::Admit);
    settle().await;
    // Requests are not held back while it hangs.
    assert_eq!(h.check().await, Decision::Admit);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        h.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, Some("result:timeout")),
        1
    );
    assert_eq!(h.snapshot().await.unwrap().2, SLICE / 2);
    assert_eq!(h.check().await, Decision::Admit);
    // With the slice used the key is still admitted this far from the limit.
    h.record(SLICE).await;
    assert_eq!(h.check().await, Decision::Admit);
    assert_eq!(h.source.reads(), 2, "the interval has not passed");
}

// --- first read ---

#[tokio::test(start_paused = true)]
async fn a_failed_first_read_is_an_error_and_nothing_is_kept() {
    let h = Harness::new(Source::new(0));
    h.source.fail(true);
    assert!(h.cache.check(h.id, LIMIT).await.is_err());
    assert!(h.snapshot().await.is_none());
    assert_eq!(h.source.reads(), 1);
    h.source.fail(false);
    assert_eq!(h.check().await, Decision::Admit);
    assert_eq!(h.source.reads(), 2);
    assert!(h.snapshot().await.is_some());
}

#[tokio::test(start_paused = true)]
async fn a_first_read_that_times_out_is_an_error() {
    let h = Harness::with_timeout(Source::new(0), Duration::from_secs(1));
    h.source.hang(true);
    assert!(h.cache.check(h.id, LIMIT).await.is_err());
    assert!(h.snapshot().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn a_cost_recorded_during_the_first_read_is_counted() {
    let gate = Arc::new(Semaphore::new(0));
    let h = Arc::new(Harness::new(Source::gated(0, gate.clone())));
    let waiting = tokio::spawn({
        let h = h.clone();
        async move { h.check().await }
    });
    settle().await;
    assert_eq!(h.source.reads(), 1);
    h.record(5).await;
    gate.add_permits(1);
    assert_eq!(waiting.await.unwrap(), Decision::Admit);
    assert_eq!(h.snapshot().await.unwrap().2, 5);
}

#[tokio::test(start_paused = true)]
async fn costs_of_keys_without_state_are_not_counted() {
    let h = Harness::new(Source::new(0));
    h.record(500).await;
    assert!(h.snapshot().await.is_none());
    assert_eq!(h.check().await, Decision::Admit);
    assert_eq!(h.snapshot().await.unwrap().2, 0);
    // Zero and negative costs add nothing.
    h.record(0).await;
    h.record(-5).await;
    assert_eq!(h.snapshot().await.unwrap().2, 0);
}

// --- concurrency ---

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_checks_share_one_read() {
    let gate = Arc::new(Semaphore::new(0));
    let h = Arc::new(Harness::new(Source::gated(0, gate.clone())));
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let h = h.clone();
            tokio::spawn(async move { h.check().await })
        })
        .collect();
    while h.source.reads() == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.source.reads(), 1, "first checks must share one read");
    gate.add_permits(100);
    for t in tasks {
        assert_eq!(t.await.unwrap(), Decision::Admit);
    }
    assert_eq!(h.source.reads(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_arriving_as_the_first_read_completes_do_not_start_another_read() {
    for _ in 0..20 {
        let gate = Arc::new(Semaphore::new(0));
        let h = Arc::new(Harness::new(Source::gated(0, gate.clone())));
        let mut tasks = Vec::new();
        let first = {
            let h = h.clone();
            tokio::spawn(async move { h.check().await })
        };
        tasks.push(first);
        while h.source.reads() == 0 {
            tokio::task::yield_now().await;
        }
        gate.add_permits(100);
        // Checks released at slightly different times around the completion.
        for i in 0..64 {
            let h = h.clone();
            tasks.push(tokio::spawn(async move {
                for _ in 0..(i % 8) {
                    tokio::task::yield_now().await;
                }
                h.check().await
            }));
            if i % 4 == 0 {
                tokio::task::yield_now().await;
            }
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), Decision::Admit);
        }
        assert_eq!(
            h.source.reads(),
            1,
            "a key is read once, however checks interleave"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_checks_past_the_soft_threshold_start_one_refresh() {
    let gate = Arc::new(Semaphore::new(1));
    let h = Arc::new(Harness::new(Source::gated(0, gate.clone())));
    assert_eq!(h.check().await, Decision::Admit);
    h.record(SLICE / 2).await;
    let tasks: Vec<_> = (0..32)
        .map(|_| {
            let h = h.clone();
            tokio::spawn(async move { h.check().await })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), Decision::Admit);
    }
    while h.source.reads() < 2 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.source.reads(), 2, "one first read plus one refresh");
    gate.add_permits(100);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.source.reads(), 2);
}

// --- the arithmetic ---

#[tokio::test(start_paused = true)]
async fn a_changed_limit_changes_only_the_arithmetic() {
    let h = Harness::new(Source::new(0));
    assert_eq!(h.check().await, Decision::Admit);
    h.record(20_000).await;
    assert_eq!(h.check().await, Decision::Admit);
    // The limit drops to 20,500: the slice is 2,050, already used, and 500
    // is left, inside the 5% band (1,025).
    let decision = h.cache.check(h.id, 20_500).await.unwrap();
    assert!(retry_after(decision) >= 1);
    // And the anchor is over a limit of zero.
    let decision = h.cache.check(h.id, 0).await.unwrap();
    assert_eq!(decision, Decision::OverLimit { spend: 0 });
}

#[tokio::test(start_paused = true)]
async fn huge_limits_and_tiny_headroom_do_not_overflow_or_stall() {
    let h = Harness::new(Source::new(i64::MIN));
    assert_eq!(
        h.cache.check(h.id, i64::MAX).await.unwrap(),
        Decision::Admit
    );
    // One unit of headroom still has a slice of one unit.
    let h = Harness::new(Source::new(LIMIT - 1));
    assert_eq!(h.check().await, Decision::Admit);
    h.record(1).await;
    assert!(retry_after(h.check().await) >= 1);
    // Recorded costs saturate instead of wrapping.
    let h = Harness::new(Source::new(0));
    assert_eq!(h.check().await, Decision::Admit);
    h.record(i64::MAX).await;
    h.record(i64::MAX).await;
    assert!(retry_after(h.check().await) >= 1);
}

// --- off mode ---

#[tokio::test]
async fn without_the_budget_check_compares_the_cached_spend() {
    let clock = FakeClock::new();
    let source = Source::new(LIMIT - 1);
    let cache = ApiKeySpendCache::new(
        10,
        Duration::from_secs(30),
        source.clone(),
        Arc::new(CapturingMetricsService::new()),
        clock.clone(),
    );
    let id = Uuid::new_v4();
    assert_eq!(cache.check(id, LIMIT).await.unwrap(), Decision::Admit);
    assert_eq!(cache.api_key_spend(id, LIMIT).await.unwrap(), LIMIT - 1);
    source.set(LIMIT);
    clock.advance(11_000);
    assert_eq!(
        cache.check(id, LIMIT).await.unwrap(),
        Decision::OverLimit { spend: LIMIT }
    );
    // Recording is a no-op, and no budget state exists.
    cache.record_cost(id, 5).await;
    assert!(cache.budget_snapshot(id).await.is_none());
    assert_eq!(source.reads(), 2);
}

// --- review follow-ups ---

#[tokio::test(start_paused = true)]
async fn tiny_headroom_with_nothing_used_does_not_refresh() {
    let h = Harness::new(Source::new(LIMIT - 1));
    assert_eq!(h.check().await, Decision::Admit);
    // Over 20 s, well past the 5 s interval and under the max age.
    for _ in 0..20 {
        h.clock.advance(1_000);
        for _ in 0..5 {
            assert_eq!(h.check().await, Decision::Admit);
        }
        settle().await;
    }
    assert_eq!(h.source.reads(), 1);
    h.record(1).await;
    assert!(retry_after(h.check().await) >= 1);
}

#[tokio::test(start_paused = true)]
async fn a_state_evicted_during_the_first_read_is_the_one_fed_afterwards() {
    let gate = Arc::new(Semaphore::new(0));
    let h = Arc::new(Harness::new(Source::gated(0, gate.clone())));
    let first = tokio::spawn({
        let h = h.clone();
        async move { h.check().await }
    });
    settle().await;
    assert_eq!(h.source.reads(), 1);
    h.cache.evict_budget_state(h.id).await;
    // A second check creates a new state and joins the read in flight.
    let second = tokio::spawn({
        let h = h.clone();
        async move { h.check().await }
    });
    settle().await;
    h.record(5).await;
    gate.add_permits(1);
    assert_eq!(first.await.unwrap(), Decision::Admit);
    assert_eq!(second.await.unwrap(), Decision::Admit);
    assert_eq!(h.source.reads(), 1);
    // The state that record_cost feeds has the anchor and the cost.
    assert_eq!(h.snapshot().await.unwrap().2, 5);
    h.record(3).await;
    assert_eq!(h.snapshot().await.unwrap().2, 8);
}

#[tokio::test(start_paused = true)]
async fn a_panicking_refresh_clears_the_in_flight_flag() {
    let h = Harness::new(Source::new(0));
    assert_eq!(h.check().await, Decision::Admit);
    h.record(SLICE / 2).await;
    h.source.panic(true);
    assert_eq!(h.check().await, Decision::Admit);
    settle().await;
    assert_eq!(h.source.reads(), 2);
    h.source.panic(false);
    h.source.set(7);
    // Before the interval: no new attempt. After it: one, which succeeds.
    h.clock.advance(4_999);
    assert_eq!(h.check().await, Decision::Admit);
    settle().await;
    assert_eq!(h.source.reads(), 2);
    h.clock.advance(1);
    assert_eq!(h.check().await, Decision::Admit);
    settle().await;
    assert_eq!(h.source.reads(), 3);
    assert_eq!(h.snapshot().await.unwrap().0, 7);
}

#[test]
fn a_cancelled_refresh_clears_the_in_flight_flag() {
    let source = Source::new(0);
    let h = Harness::new(source.clone());
    let key = h.id;
    let first = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    first.block_on(async {
        assert_eq!(h.check().await, Decision::Admit);
        h.record(SLICE / 2).await;
        source.hang(true);
        assert_eq!(h.check().await, Decision::Admit);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(source.reads(), 2);
    });
    // Dropping the runtime cancels the hanging refresh task.
    drop(first);
    source.hang(false);
    source.set(9);
    h.clock.advance(5_000);
    let second = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    second.block_on(async {
        assert_eq!(h.cache.check(key, LIMIT).await.unwrap(), Decision::Admit);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(source.reads(), 3);
        assert_eq!(h.snapshot().await.unwrap().0, 9);
    });
}

#[tokio::test]
async fn idle_state_is_rebuilt_by_one_blocking_read() {
    // Real time: the state cache expires idle entries on its own clock.
    let gate = Arc::new(Semaphore::new(1));
    let h = Arc::new(Harness::with_settings(
        Source::gated(0, gate.clone()),
        Duration::from_secs(30),
        1,
    ));
    assert_eq!(h.check().await, Decision::Admit);
    h.record(10).await;
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    h.clock.advance(1_300);
    let waiting = tokio::spawn({
        let h = h.clone();
        async move { h.check().await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The request is held by the read: it is not a background refresh.
    assert!(!waiting.is_finished());
    assert_eq!(h.source.reads(), 2);
    gate.add_permits(1);
    assert_eq!(waiting.await.unwrap(), Decision::Admit);
    assert_eq!(h.snapshot().await.unwrap(), (0, T0 + 1_300, 0));
    assert_eq!(h.source.reads(), 2);
}
