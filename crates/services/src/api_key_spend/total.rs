//! The shared running total: request-path logic and failure handling.
//!
//! See the module docs of [`super`] for the protocol. This file owns the
//! parts that must never affect a request's result: timeouts, the pause after
//! a store failure, the bounded background work, and the error counters.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use uuid::Uuid;

use super::ports::{Clock, SpendTotalStore, StoreErrorKind, TotalRead};
use super::Entry;
use crate::metrics::consts::{
    METRIC_API_KEY_SPEND_TOTAL_BG_DROPPED, METRIC_API_KEY_SPEND_TOTAL_CONFIRM,
    METRIC_API_KEY_SPEND_TOTAL_ERROR, METRIC_API_KEY_SPEND_TOTAL_HIT,
    METRIC_API_KEY_SPEND_TOTAL_INVALIDATE, METRIC_API_KEY_SPEND_TOTAL_MALFORMED,
    METRIC_API_KEY_SPEND_TOTAL_MISS, METRIC_API_KEY_SPEND_TOTAL_PAUSED,
    METRIC_API_KEY_SPEND_TOTAL_SEED, METRIC_API_KEY_SPEND_TOTAL_SHADOW,
    METRIC_API_KEY_SPEND_TOTAL_SHADOW_DIFF, TAG_KIND, TAG_RESULT,
};
use crate::metrics::MetricsServiceTrait;

/// Timeout of the store calls on the request path (GET_TOTAL, BEGIN).
pub const REQUEST_TIMEOUT: Duration = Duration::from_millis(50);
/// Timeout of each store call made off the request path.
pub const BACKGROUND_TIMEOUT: Duration = Duration::from_millis(500);
/// A seed older than this (BEGIN to the database reading) is not finished: its
/// pending costs may have expired.
pub const MAX_SEED_AGE: Duration = Duration::from_secs(90);
/// How long the shared level is skipped after a store failure.
pub const PAUSE_AFTER_ERROR: Duration = Duration::from_secs(5);
/// ... and after `NOPERM`, which does not heal by itself.
pub const PAUSE_AFTER_NOPERM: Duration = Duration::from_secs(60);
/// Most background tasks (FINISH, INVALIDATE, shadow compare) in flight.
pub const MAX_BACKGROUND_TASKS: usize = 64;
const LOG_EVERY_MS: u64 = 60_000;

/// What the shared level does with its total.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TotalMode {
    /// Decisions still use the database; the total is maintained and compared.
    Shadow,
    /// A total below the limit replaces the database read.
    On,
}

pub struct SharedTotal {
    store: Arc<dyn SpendTotalStore>,
    clock: Arc<dyn Clock>,
    metrics: Arc<dyn MetricsServiceTrait>,
    mode: TotalMode,
    paused_until_ms: AtomicU64,
    /// Time of the last error log line; `0` before the first.
    last_log_ms: AtomicU64,
    background: Arc<Semaphore>,
}

impl SharedTotal {
    pub fn new(
        store: Arc<dyn SpendTotalStore>,
        clock: Arc<dyn Clock>,
        metrics: Arc<dyn MetricsServiceTrait>,
        mode: TotalMode,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            clock,
            metrics,
            mode,
            paused_until_ms: AtomicU64::new(0),
            last_log_ms: AtomicU64::new(0),
            background: Arc::new(Semaphore::new(MAX_BACKGROUND_TASKS)),
        })
    }

    pub fn mode(&self) -> TotalMode {
        self.mode
    }

    pub(super) fn store(&self) -> &Arc<dyn SpendTotalStore> {
        &self.store
    }

    pub(super) fn count(&self, name: &str, tags: &[&str]) {
        self.metrics.record_count(name, 1, tags);
    }

    pub(super) fn count_result(&self, name: &str, result: &str) {
        self.count(name, &[&format!("{TAG_RESULT}:{result}")]);
    }

    /// After a store failure the shared level is skipped for a few seconds, so
    /// an unreachable store costs one timeout, not one per request.
    pub fn paused(&self) -> bool {
        self.clock.now_ms() < self.paused_until_ms.load(Ordering::Relaxed)
    }

    /// Counts a store failure by kind, pauses the shared level, and logs at
    /// most once a minute (the kind only).
    pub(super) fn fail(&self, kind: StoreErrorKind) {
        self.count(
            METRIC_API_KEY_SPEND_TOTAL_ERROR,
            &[&format!("{TAG_KIND}:{}", kind.as_str())],
        );
        let now = self.clock.now_ms();
        let pause = if kind == StoreErrorKind::NoPerm {
            PAUSE_AFTER_NOPERM
        } else {
            PAUSE_AFTER_ERROR
        };
        self.paused_until_ms
            .fetch_max(now + pause.as_millis() as u64, Ordering::Relaxed);
        let last = self.last_log_ms.load(Ordering::Relaxed);
        if (last == 0 || now.saturating_sub(last) >= LOG_EVERY_MS)
            && self
                .last_log_ms
                .compare_exchange(last, now.max(1), Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(
                error_kind = kind.as_str(),
                "API key spend total store call failed; using the database for a while"
            );
        }
    }

    /// Runs one store call under `timeout`. A failure (including the timeout)
    /// is recorded with [`Self::fail`] and returned as `None`.
    pub(super) async fn call<T>(
        &self,
        timeout: Duration,
        call: impl Future<Output = Result<T, StoreErrorKind>>,
    ) -> Option<T> {
        match tokio::time::timeout(timeout, call).await {
            Ok(Ok(value)) => Some(value),
            Ok(Err(kind)) => {
                self.fail(kind);
                None
            }
            Err(_) => {
                self.fail(StoreErrorKind::Timeout);
                None
            }
        }
    }

    /// Spawns `work` if fewer than [`MAX_BACKGROUND_TASKS`] are in flight;
    /// otherwise drops it and counts the drop.
    pub(super) fn spawn_background<F>(&self, work: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        match self.background.clone().try_acquire_owned() {
            Ok(permit) => {
                tokio::spawn(async move {
                    work.await;
                    drop(permit);
                });
                true
            }
            Err(_) => {
                self.count(METRIC_API_KEY_SPEND_TOTAL_BG_DROPPED, &[]);
                false
            }
        }
    }

    /// The spend reading for one limit check, coalesced by the caller's
    /// cache. `read_db` is the authoritative database read.
    ///
    /// The shared total is a best-effort cache maintained by increments, so a
    /// rejection is always confirmed against the database: a total at or over
    /// the limit is never used itself. Every store failure falls back to
    /// `read_db`.
    pub(super) async fn load<F, Fut>(
        self: &Arc<Self>,
        api_key_id: Uuid,
        limit: i64,
        read_db: F,
    ) -> anyhow::Result<Entry>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<Entry>>,
    {
        if self.paused() {
            self.count(METRIC_API_KEY_SPEND_TOTAL_PAUSED, &[]);
            return read_db().await;
        }
        // Taken before the call, so the age never looks younger than it is.
        let read_at_ms = self.clock.now_ms();
        let read = self
            .call(REQUEST_TIMEOUT, self.store.get_total(api_key_id))
            .await;
        match (self.mode, read) {
            (_, None) => read_db().await,
            (TotalMode::On, Some(TotalRead::Value(total))) if total < limit => {
                self.count(METRIC_API_KEY_SPEND_TOTAL_HIT, &[]);
                Ok(Entry {
                    spend: total,
                    read_at_ms,
                    from_db: false,
                })
            }
            (TotalMode::On, Some(TotalRead::Value(_))) => {
                self.count(METRIC_API_KEY_SPEND_TOTAL_CONFIRM, &[]);
                let entry = read_db().await?;
                if entry.spend < limit {
                    // The total overcounted: drop it so the next check
                    // re-seeds it from the database.
                    self.count(METRIC_API_KEY_SPEND_TOTAL_INVALIDATE, &[]);
                    self.spawn_invalidate(api_key_id);
                }
                Ok(entry)
            }
            (TotalMode::Shadow, Some(TotalRead::Value(_))) => {
                let entry = read_db().await?;
                self.spawn_compare(api_key_id, entry.spend);
                Ok(entry)
            }
            (_, Some(TotalRead::Malformed)) => {
                self.count(METRIC_API_KEY_SPEND_TOTAL_MALFORMED, &[]);
                self.seed(api_key_id, read_db).await
            }
            (_, Some(TotalRead::Missing)) => {
                self.count(METRIC_API_KEY_SPEND_TOTAL_MISS, &[]);
                self.seed(api_key_id, read_db).await
            }
        }
    }

    /// BEGIN, the database read, FINISH (off the request path). The result is
    /// the database reading, whatever the store does.
    async fn seed<F, Fut>(self: &Arc<Self>, api_key_id: Uuid, read_db: F) -> anyhow::Result<Entry>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<Entry>>,
    {
        let generation = self
            .call(REQUEST_TIMEOUT, self.store.begin(api_key_id))
            .await;
        let began_ms = self.clock.now_ms();
        let entry = read_db().await?;
        match generation {
            Some(generation)
                if self.clock.now_ms().saturating_sub(began_ms)
                    <= MAX_SEED_AGE.as_millis() as u64 =>
            {
                self.spawn_finish(api_key_id, entry.spend, generation);
            }
            Some(_) => self.count_result(METRIC_API_KEY_SPEND_TOTAL_SEED, "too_old"),
            None => {}
        }
        Ok(entry)
    }

    fn spawn_finish(self: &Arc<Self>, api_key_id: Uuid, sum: i64, generation: String) {
        let this = self.clone();
        self.spawn_background(async move {
            let result = this
                .call(
                    BACKGROUND_TIMEOUT,
                    this.store.finish(api_key_id, sum, &generation),
                )
                .await;
            match result {
                Some(Some(_)) => this.count_result(METRIC_API_KEY_SPEND_TOTAL_SEED, "written"),
                Some(None) => this.count_result(METRIC_API_KEY_SPEND_TOTAL_SEED, "aborted"),
                None => this.count_result(METRIC_API_KEY_SPEND_TOTAL_SEED, "error"),
            }
        });
    }

    fn spawn_invalidate(self: &Arc<Self>, api_key_id: Uuid) {
        let this = self.clone();
        self.spawn_background(async move {
            this.call(BACKGROUND_TIMEOUT, this.store.invalidate(api_key_id))
                .await;
        });
    }

    /// Shadow mode: reads the total again after the database reading and
    /// records how it compares (`under` = the total is below the database).
    fn spawn_compare(self: &Arc<Self>, api_key_id: Uuid, database_spend: i64) {
        let this = self.clone();
        self.spawn_background(async move {
            let read = this
                .call(BACKGROUND_TIMEOUT, this.store.get_total(api_key_id))
                .await;
            if let Some(TotalRead::Value(total)) = read {
                let result = match total.cmp(&database_spend) {
                    std::cmp::Ordering::Equal => "equal",
                    std::cmp::Ordering::Less => "under",
                    std::cmp::Ordering::Greater => "over",
                };
                this.count_result(METRIC_API_KEY_SPEND_TOTAL_SHADOW, result);
                this.metrics.record_histogram(
                    METRIC_API_KEY_SPEND_TOTAL_SHADOW_DIFF,
                    total.abs_diff(database_spend) as f64,
                    &[],
                );
            }
        });
    }
}
