//! Cache in front of the per-request API-key spend check.
//!
//! The check reads the key's lifetime spend (`SUM(total_cost)` over its whole
//! usage history) on every inference request, which gets expensive for keys
//! with a long history. This module puts an in-process [`moka`] cache keyed by
//! API key id in front of that read. Concurrent misses for one key run a
//! single load (moka's coalescing loader), and a reading is reused for a
//! short time.
//!
//! # Staleness bound
//!
//! A cached reading is used while its age is at most the TTL (default 10 s;
//! `0` disables the cache). Age is measured on a monotonic clock, so a
//! wall-clock step cannot make an old reading look fresh. A reading near the
//! limit, at the limit or over it must be at most [`NEAR_LIMIT_MAX_AGE_MS`]
//! old when it is looked up; see [`required_max_age_ms`]. Spend can also move
//! down (a usage cost can be rewritten), so an over-limit reading is not kept
//! for the TTL: once its spend drops below the limit a key stops being
//! rejected within that second plus the time one read takes.
//!
//! A reading handed to a caller that waited for its load is used as it
//! arrives, so it can be older than the bound by the time that read took
//! (age is counted from the start of the read). Later callers judge the age
//! from the same start, so when reads take longer than a second, a near-limit
//! or over-limit key that keeps sending requests runs reads back to back, one
//! at a time per process (coalesced), not one per request. Otherwise such a
//! key costs at most one coalesced read per second per process.
//!
//! One read is bounded by a timeout (`API_KEY_SPEND_READ_TIMEOUT_SECS`). A
//! read that exceeds it fails, is not cached, and reaches every caller that
//! was waiting for it as an error.
//!
//! The check only reads: nothing reserves spend for requests that are still
//! running, and a request's cost is written when it finishes, so a long
//! stream adds nothing until it ends. A key can therefore overshoot its limit
//! by what its admitted requests spend while a reading is reused (up to one
//! TTL) plus what requests already running will still spend, which is bounded
//! by their own duration. The near-limit rule narrows this only for keys
//! whose last reading is already within 5% of the limit; a reading further
//! below is reused for the full TTL (limit 100, cached reading 90, a burst
//! spending 60 inside the TTL ends at 150). Requests in flight overshoot even
//! without the cache; the cache adds up to one TTL of the key's spend rate on
//! top. Each process keeps its own readings, so the bound applies per process.
//!
//! # Local budget mode
//!
//! With `API_KEY_SPEND_BUDGET_MODE=on` the check does not reuse a reading for a
//! TTL. It reads the database only when a key has used a slice of its
//! remaining headroom, and can answer 429 near the limit. The rule, its
//! overshoot bound and its settings are in [`budget`]; the costs it counts
//! come from [`recorder`]. [`ApiKeySpendCache::check`] is the entry point for
//! both modes.
//!
//! # Privacy
//!
//! Nothing is logged here. Metrics carry no tags except the closed-set
//! `result` tag of the budget refresh counter.

pub mod adapters;
pub mod budget;
#[cfg(test)]
mod budget_tests;
pub mod ports;
pub mod recorder;
#[cfg(test)]
mod recorder_tests;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::{Duration, Instant};

use moka::future::Cache;
use moka::ops::compute::Op;
use uuid::Uuid;

use crate::metrics::consts::{
    METRIC_API_KEY_SPEND_DB_READ, METRIC_API_KEY_SPEND_DB_READ_MS,
    METRIC_API_KEY_SPEND_DB_READ_TIMEOUT, METRIC_API_KEY_SPEND_L1_HIT,
    METRIC_API_KEY_SPEND_NEAR_LIMIT_REFRESH,
};
use crate::metrics::MetricsServiceTrait;
use budget::{Budget, BudgetSettings};
use ports::{ApiKeySpendSource, Clock};

/// A reading within 5% of the limit (`1 / NEAR_LIMIT_MARGIN_DIVISOR` of it) is
/// "near the limit" and must be at most [`NEAR_LIMIT_MAX_AGE_MS`] old.
pub const NEAR_LIMIT_MARGIN_DIVISOR: i64 = 20;
/// Maximum age of a reading that is near the limit, at the limit or over it
/// (`limit - spend <= max(limit / 20, 0)`, which holds whenever `spend >=
/// limit`). Older readings force a fresh, still coalesced, read. This keeps
/// the overshoot of a key close to its limit small even with a long TTL, and
/// stops an over-limit rejection from outliving a downward change in spend.
pub const NEAR_LIMIT_MAX_AGE_MS: u64 = 1_000;

/// Capacity of the in-process cache (entries are two words).
const L1_CAPACITY: u64 = 100_000;

/// One reading of a key's spend and when it was read (monotonic clock, taken
/// before the read started, so it never understates the age).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    spend: i64,
    read_at_ms: u64,
}

impl Entry {
    fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.read_at_ms)
    }
}

/// What the spend check decided for one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Within the limit.
    Admit,
    /// The key has used the slice of headroom this process may spend between
    /// readings; answer 429 and tell the caller to retry after this many
    /// whole seconds (at least 1).
    Throttle { retry_after_secs: u64 },
    /// The key's spend has reached the limit.
    OverLimit { spend: i64 },
}

/// Why one database read failed.
pub(crate) enum ReadFailure {
    Timeout,
    Source(anyhow::Error),
}

impl ReadFailure {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Timeout => anyhow::anyhow!("API key spend read timed out"),
            Self::Source(e) => e,
        }
    }
}

/// How fresh a reading must be for one limit check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Freshness {
    pub max_age_ms: u64,
    /// The short bound applies: the reading is near the limit, at it or over it.
    pub near_limit: bool,
}

/// The freshness rule. If `limit - spend <= max(limit / 20, 0)` (within 5% of
/// the limit, at the limit or over it) the reading must be at most
/// [`NEAR_LIMIT_MAX_AGE_MS`] old, capped by the TTL, and `near_limit` is set
/// to mean "the short bound applies". Everything else may be as old as the
/// TTL. The margin is a division, so it cannot overflow for large limits.
pub fn required_max_age_ms(spend: i64, limit: i64, ttl_ms: u64) -> Freshness {
    let margin = (limit / NEAR_LIMIT_MARGIN_DIVISOR).max(0);
    if limit.saturating_sub(spend) <= margin {
        Freshness {
            max_age_ms: NEAR_LIMIT_MAX_AGE_MS.min(ttl_ms),
            near_limit: true,
        }
    } else {
        Freshness {
            max_age_ms: ttl_ms,
            near_limit: false,
        }
    }
}

struct Inner {
    ttl_ms: u64,
    read_timeout: Duration,
    source: Arc<dyn ApiKeySpendSource>,
    metrics: Arc<dyn MetricsServiceTrait>,
    clock: Arc<dyn Clock>,
    l1: Cache<Uuid, Entry>,
    /// Present in local budget mode.
    budget: Option<Budget>,
}

/// The spend lookup used by the API-key spend-limit check.
#[derive(Clone)]
pub struct ApiKeySpendCache {
    inner: Arc<Inner>,
}

impl ApiKeySpendCache {
    /// `ttl_secs == 0` disables the cache: every call reads `source`. Every
    /// read of `source` is abandoned after `read_timeout`.
    pub fn new(
        ttl_secs: u64,
        read_timeout: Duration,
        source: Arc<dyn ApiKeySpendSource>,
        metrics: Arc<dyn MetricsServiceTrait>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::new_with_budget(ttl_secs, read_timeout, source, metrics, clock, None)
    }

    /// Like [`Self::new`]; with `budget` set, [`Self::check`] uses the local
    /// budget instead of the TTL cache.
    pub fn new_with_budget(
        ttl_secs: u64,
        read_timeout: Duration,
        source: Arc<dyn ApiKeySpendSource>,
        metrics: Arc<dyn MetricsServiceTrait>,
        clock: Arc<dyn Clock>,
        budget: Option<BudgetSettings>,
    ) -> Self {
        let ttl = Duration::from_secs(ttl_secs);
        // The time-to-live only frees memory; freshness is judged from
        // `read_at_ms` on every lookup.
        let l1 = Cache::builder()
            .max_capacity(L1_CAPACITY)
            .time_to_live(ttl.max(Duration::from_secs(1)) + Duration::from_secs(1))
            .build();
        Self {
            inner: Arc::new(Inner {
                ttl_ms: ttl_secs.saturating_mul(1_000),
                read_timeout,
                source,
                metrics,
                clock,
                l1,
                budget: budget.map(Budget::new),
            }),
        }
    }

    /// The decision for one request against `limit`. Without the local budget
    /// the cached spend is compared with the limit; with it, see [`budget`].
    /// The error is the database read's, shared by every caller that was
    /// waiting on the same read.
    pub async fn check(
        &self,
        api_key_id: Uuid,
        limit: i64,
    ) -> Result<Decision, Arc<anyhow::Error>> {
        if self.inner.budget.is_some() {
            return self.check_budget(api_key_id, limit).await;
        }
        let spend = self.api_key_spend(api_key_id, limit).await?;
        Ok(if spend >= limit {
            Decision::OverLimit { spend }
        } else {
            Decision::Admit
        })
    }

    /// The key's spend for a check against `limit`, at most as stale as the
    /// freshness rule allows. The error is the database read's, shared by
    /// every caller that was waiting on the same load.
    pub async fn api_key_spend(
        &self,
        api_key_id: Uuid,
        limit: i64,
    ) -> Result<i64, Arc<anyhow::Error>> {
        let inner = &self.inner;
        if inner.ttl_ms == 0 {
            return inner
                .read_db(api_key_id)
                .await
                .map(|e| e.spend)
                .map_err(Arc::new);
        }
        if let Some(entry) = inner.l1.get(&api_key_id).await {
            let age = entry.age_ms(inner.clock.now_ms());
            let need = required_max_age_ms(entry.spend, limit, inner.ttl_ms);
            if age <= need.max_age_ms {
                inner.count(METRIC_API_KEY_SPEND_L1_HIT, &[]);
                return Ok(entry.spend);
            }
            if need.near_limit && age <= inner.ttl_ms {
                inner.count(METRIC_API_KEY_SPEND_NEAR_LIMIT_REFRESH, &[]);
            }
            inner.remove_if_same(api_key_id, entry).await;
        }
        // The reading comes from a load this call waited for: its own, one it
        // joined, or one that finished between the lookup above and this
        // load. It is the freshest this call can get from one read, so it is
        // used whatever the read took.
        let entry = inner
            .l1
            .try_get_with(api_key_id, inner.read_db(api_key_id))
            .await?;
        Ok(entry.spend)
    }
}

impl Inner {
    fn count(&self, name: &str, tags: &[&str]) {
        self.metrics.record_count(name, 1, tags);
    }

    /// Removes the cached entry only if it is still `stale`, so a concurrent
    /// caller's fresh reload is never thrown away.
    async fn remove_if_same(&self, api_key_id: Uuid, stale: Entry) {
        self.l1
            .entry(api_key_id)
            .and_compute_with(|current| async move {
                match current {
                    Some(e) if *e.value() == stale => Op::Remove,
                    _ => Op::Nop,
                }
            })
            .await;
    }

    async fn read_db(&self, api_key_id: Uuid) -> anyhow::Result<Entry> {
        self.read_classified(api_key_id)
            .await
            .map_err(ReadFailure::into_anyhow)
    }

    /// One bounded read; says whether a failure was the timeout.
    async fn read_classified(&self, api_key_id: Uuid) -> Result<Entry, ReadFailure> {
        let read_at_ms = self.clock.now_ms();
        let started = Instant::now();
        let result =
            match tokio::time::timeout(self.read_timeout, self.source.api_key_spend(api_key_id))
                .await
            {
                Ok(result) => result.map_err(ReadFailure::Source),
                Err(_) => {
                    self.count(METRIC_API_KEY_SPEND_DB_READ_TIMEOUT, &[]);
                    Err(ReadFailure::Timeout)
                }
            };
        self.metrics.record_histogram(
            METRIC_API_KEY_SPEND_DB_READ_MS,
            started.elapsed().as_secs_f64() * 1_000.0,
            &[],
        );
        self.count(METRIC_API_KEY_SPEND_DB_READ, &[]);
        result.map(|spend| Entry { spend, read_at_ms })
    }
}
