//! Shared fakes for the tests of this module.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::ports::{ApiKeySpendSource, Clock};
use super::{ApiKeySpendCache, Decision};
use crate::metrics::capturing::{CapturingMetricsService, MetricValue};

pub(super) const T0: u64 = 1_700_000_000_000;
pub(super) const LIMIT: i64 = 1_000_000;

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

/// Counts reads; can hold them, delay them, fail them, panic or never finish.
pub(super) struct Source {
    spend: AtomicI64,
    reads: AtomicUsize,
    gate: Option<Arc<Semaphore>>,
    delay_ms: AtomicU64,
    fail: AtomicBool,
    hang: AtomicBool,
    panic: AtomicBool,
}

impl Source {
    pub(super) fn new(spend: i64) -> Arc<Self> {
        Self::build(spend, None)
    }
    pub(super) fn gated(spend: i64, gate: Arc<Semaphore>) -> Arc<Self> {
        Self::build(spend, Some(gate))
    }
    fn build(spend: i64, gate: Option<Arc<Semaphore>>) -> Arc<Self> {
        Arc::new(Self {
            spend: AtomicI64::new(spend),
            reads: AtomicUsize::new(0),
            gate,
            delay_ms: AtomicU64::new(0),
            fail: AtomicBool::new(false),
            hang: AtomicBool::new(false),
            panic: AtomicBool::new(false),
        })
    }
    pub(super) fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
    pub(super) fn set(&self, spend: i64) {
        self.spend.store(spend, Ordering::SeqCst);
    }
    pub(super) fn delay_ms(&self, ms: u64) {
        self.delay_ms.store(ms, Ordering::SeqCst);
    }
    pub(super) fn fail(&self, fail: bool) {
        self.fail.store(fail, Ordering::SeqCst);
    }
    pub(super) fn panic(&self, panic: bool) {
        self.panic.store(panic, Ordering::SeqCst);
    }
    pub(super) fn hang(&self, hang: bool) {
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
        let delay = self.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
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

pub(super) struct Harness {
    pub(super) cache: ApiKeySpendCache,
    pub(super) source: Arc<Source>,
    pub(super) clock: Arc<FakeClock>,
    pub(super) metrics: Arc<CapturingMetricsService>,
    pub(super) id: Uuid,
}

impl Harness {
    /// The budget on, with a max age of 60 s.
    pub(super) fn new(source: Arc<Source>) -> Self {
        Self::with_budget(source, Some(60))
    }

    /// The budget off: every check reads the source.
    pub(super) fn off(source: Arc<Source>) -> Self {
        Self::with_budget(source, None)
    }

    pub(super) fn with_budget(source: Arc<Source>, max_age_secs: Option<u64>) -> Self {
        let clock = FakeClock::new();
        let metrics = Arc::new(CapturingMetricsService::new());
        let cache =
            ApiKeySpendCache::new(source.clone(), metrics.clone(), clock.clone(), max_age_secs);
        Self {
            cache,
            source,
            clock,
            metrics,
            id: Uuid::new_v4(),
        }
    }

    pub(super) async fn check(&self) -> Decision {
        self.cache.check(self.id, LIMIT).await.unwrap()
    }

    pub(super) async fn record(&self, cost: i64) {
        self.cache.record_cost(self.id, cost).await;
    }

    /// The key's anchor spend, anchor time and `used`.
    pub(super) async fn snapshot(&self) -> Option<(i64, u64, i64)> {
        self.cache.budget_snapshot(self.id).await
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
