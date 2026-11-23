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
//! `0` disables the cache). A reading near the limit must be at most
//! [`NEAR_LIMIT_MAX_AGE_MS`] old; see [`required_max_age_ms`]. Spend only
//! grows, so a reading at or over the limit may reject for the whole TTL. The
//! consequence of the TTL: a key can overshoot its limit by what it spends
//! within one TTL plus one request in flight, bounded further by the 5% rule
//! above. Each process keeps its own readings, so the bound applies per
//! process.
//!
//! # Shared running total
//!
//! Each process still runs the database `SUM` once per key per TTL. With
//! `API_KEY_SPEND_TOTAL_MODE=on` the processes also share a running total per
//! key in the placement Valkey, so the `SUM` runs about once per key per
//! re-check interval (`shadow` maintains and compares the total but decides
//! from the database; `off` runs nothing of this). The key names, expiries and
//! scripts are in [`lua`].
//!
//! Why it is safe to be wrong: the total is maintained by best-effort
//! increments and is not authoritative, so a rejection is always confirmed
//! against the database. A total below the limit is used as is. A total at or
//! over the limit is never used: the database is read and its reading is
//! used, and if that reading is below the limit the total is dropped. An
//! in-process entry at or over the limit therefore always came from the
//! database (`Entry::from_db`).
//!
//! Seeding a missing total (the protocol of [`ports::SpendTotalStore`]):
//! BEGIN starts collecting costs as *pending*, then the database `SUM` runs,
//! then FINISH writes `SUM + pending`. A usage row sends ADD after it commits,
//! and ADD updates the total and the pending costs only where they exist. A
//! row committed after the `SUM` snapshot is therefore in pending; a row
//! committed just before the snapshot but added after BEGIN is counted twice,
//! which only overcounts (the safe side). A generation counter lets only the
//! first finisher after a quiet state write the total: a seeder whose state
//! was invalidated or re-seeded meanwhile aborts and keeps its own database
//! reading. Anything that may have lost a cost (a failed or dropped ADD)
//! invalidates the total, which re-seeds from the database.
//!
//! Any store failure falls back to the database and pauses the shared level
//! for a few seconds ([`total::PAUSE_AFTER_ERROR`]).
//!
//! # Privacy
//!
//! Nothing customer-related is logged here: the only log line carries a store
//! error kind. Metrics are tagged only from closed sets (`kind`, `result`).

pub mod adapters;
#[cfg(test)]
mod fake_store;
pub mod lua;
pub mod ports;
pub mod recorder;
#[cfg(test)]
mod recorder_tests;
#[cfg(test)]
mod tests;
pub mod total;
#[cfg(test)]
mod total_tests;
pub mod valkey_store;
#[cfg(test)]
mod valkey_store_tests;

pub use recorder::{SpendTotalRecorder, SpendTotalUsageRepository};
pub use total::{SharedTotal, TotalMode};

use std::sync::Arc;
use std::time::{Duration, Instant};

use moka::future::Cache;
use moka::ops::compute::Op;
use uuid::Uuid;

use crate::metrics::consts::{
    METRIC_API_KEY_SPEND_DB_READ, METRIC_API_KEY_SPEND_DB_READ_MS, METRIC_API_KEY_SPEND_L1_HIT,
    METRIC_API_KEY_SPEND_NEAR_LIMIT_REFRESH,
};
use crate::metrics::MetricsServiceTrait;
use ports::{ApiKeySpendSource, Clock};

/// A reading within this share of the limit (as a percentage of the limit) is
/// "near the limit" and must be at most [`NEAR_LIMIT_MAX_AGE_MS`] old.
pub const NEAR_LIMIT_MARGIN_PERCENT: i64 = 5;
/// Maximum age of a reading that is near the limit (`limit - spend <=
/// max(limit * 5%, 0)`, spend still under the limit). Older readings force a
/// fresh, still coalesced, read. This keeps the overshoot of a key close to
/// its limit small even with a long TTL.
pub const NEAR_LIMIT_MAX_AGE_MS: u64 = 1_000;

/// Capacity of the in-process cache (entries are two words).
const L1_CAPACITY: u64 = 100_000;

/// One reading of a key's spend and when it was read (wall clock, taken
/// before the read started, so it never understates the age).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    spend: i64,
    read_at_ms: u64,
    /// The reading came from the database, not from the shared total.
    from_db: bool,
}

impl Entry {
    fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.read_at_ms)
    }

    /// Whether this reading may stand for a check against `limit`: one at or
    /// over the limit rejects, so it must have come from the database. (A
    /// reading from the shared total was below the limit when it was taken,
    /// but the limit may have been lowered since.)
    fn may_decide(&self, limit: i64) -> bool {
        self.from_db || self.spend < limit
    }
}

/// How fresh a reading must be for one limit check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Freshness {
    pub max_age_ms: u64,
    pub near_limit: bool,
}

/// The freshness rule. A reading at or over the limit may reject for the whole
/// TTL (spend only grows). Otherwise, if `limit - spend <= max(limit * 5%, 0)`
/// the reading must be at most [`NEAR_LIMIT_MAX_AGE_MS`] old. Everything else
/// may be as old as the TTL.
pub fn required_max_age_ms(spend: i64, limit: i64, ttl_ms: u64) -> Freshness {
    if spend >= limit {
        return Freshness {
            max_age_ms: ttl_ms,
            near_limit: false,
        };
    }
    let margin = limit
        .saturating_mul(NEAR_LIMIT_MARGIN_PERCENT)
        .saturating_div(100)
        .max(0);
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
    source: Arc<dyn ApiKeySpendSource>,
    metrics: Arc<dyn MetricsServiceTrait>,
    clock: Arc<dyn Clock>,
    l1: Cache<Uuid, Entry>,
    shared: Option<Arc<SharedTotal>>,
}

/// The spend lookup used by the API-key spend-limit check.
#[derive(Clone)]
pub struct ApiKeySpendCache {
    inner: Arc<Inner>,
}

impl ApiKeySpendCache {
    /// `ttl_secs == 0` disables the cache: every call reads `source`.
    pub fn new(
        ttl_secs: u64,
        source: Arc<dyn ApiKeySpendSource>,
        metrics: Arc<dyn MetricsServiceTrait>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::new_with_total(ttl_secs, source, metrics, clock, None)
    }

    /// As [`Self::new`], with the shared running total (`None` = no shared
    /// level: behaviour identical to [`Self::new`]).
    pub fn new_with_total(
        ttl_secs: u64,
        source: Arc<dyn ApiKeySpendSource>,
        metrics: Arc<dyn MetricsServiceTrait>,
        clock: Arc<dyn Clock>,
        shared: Option<Arc<SharedTotal>>,
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
                source,
                metrics,
                clock,
                l1,
                shared,
            }),
        }
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
        // Second pass only when the load this call joined produced a reading
        // older than this call's own bound (another caller, far from its
        // limit, started it): it reloads once and uses that result.
        for pass in 0..2 {
            if let Some(entry) = inner.l1.get(&api_key_id).await {
                let age = entry.age_ms(inner.clock.now_ms());
                let need = required_max_age_ms(entry.spend, limit, inner.ttl_ms);
                if age <= need.max_age_ms && entry.may_decide(limit) {
                    if pass == 0 {
                        inner.count(METRIC_API_KEY_SPEND_L1_HIT, &[]);
                    }
                    return Ok(entry.spend);
                }
                if pass == 0 && need.near_limit && age <= inner.ttl_ms {
                    inner.count(METRIC_API_KEY_SPEND_NEAR_LIMIT_REFRESH, &[]);
                }
                inner.remove_if_same(api_key_id, entry).await;
            }
            let entry = inner
                .l1
                .try_get_with(api_key_id, inner.load(api_key_id, limit))
                .await?;
            let need = required_max_age_ms(entry.spend, limit, inner.ttl_ms);
            if entry.may_decide(limit)
                && (pass == 1 || entry.age_ms(inner.clock.now_ms()) <= need.max_age_ms)
            {
                return Ok(entry.spend);
            }
        }
        // The second pass returns above unless it joined a load that produced
        // a reading from the shared total at or over this call's limit. Read
        // the database directly then (and rather than panic if that ever
        // stops being true).
        inner
            .read_db(api_key_id)
            .await
            .map(|e| e.spend)
            .map_err(Arc::new)
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

    /// The loader behind the cache: the shared total when there is one, the
    /// database otherwise.
    async fn load(&self, api_key_id: Uuid, limit: i64) -> anyhow::Result<Entry> {
        match &self.shared {
            Some(shared) => {
                shared
                    .load(api_key_id, limit, || self.read_db(api_key_id))
                    .await
            }
            None => self.read_db(api_key_id).await,
        }
    }

    async fn read_db(&self, api_key_id: Uuid) -> anyhow::Result<Entry> {
        let read_at_ms = self.clock.now_ms();
        let started = Instant::now();
        let result = self.source.api_key_spend(api_key_id).await;
        self.metrics.record_histogram(
            METRIC_API_KEY_SPEND_DB_READ_MS,
            started.elapsed().as_secs_f64() * 1_000.0,
            &[],
        );
        self.count(METRIC_API_KEY_SPEND_DB_READ, &[]);
        result.map(|spend| Entry {
            spend,
            read_at_ms,
            from_db: true,
        })
    }
}
