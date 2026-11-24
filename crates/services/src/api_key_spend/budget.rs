//! The local budget: the spend check without a timer.
//!
//! The database read behind the spend check sums a key's whole usage history.
//! In this mode (`API_KEY_SPEND_BUDGET_MODE=on`) a process reads it only when a
//! key has used up a slice of its remaining headroom, not on a timer per
//! request stream. Enforcement is approximate by design.
//!
//! # The rule
//!
//! Per process and per API key that has a limit, the process keeps an anchor:
//! a database reading (`anchor_spend`) and the monotonic time taken before
//! that read started. It also keeps `used`, the sum of the costs this process
//! recorded for the key since the anchor's read started. (A cost recorded
//! while the read ran may be counted twice, in the reading and in `used`;
//! that only overcounts, which is the safe direction.) For a request against
//! `limit`:
//!
//! 1. With no state yet, one blocking read, shared by every concurrent first
//!    check and bounded by the read timeout, sets the anchor. If it fails the
//!    error is returned and nothing is kept.
//! 2. `anchor_spend >= limit`: [`Decision::OverLimit`]. When the anchor is at
//!    least [`OVER_LIMIT_RECHECK`] old a background refresh starts, so a key
//!    whose spend went down is admitted again.
//! 3. Otherwise `headroom = limit - anchor_spend` and
//!    `slice = headroom * share_percent / 100` (at least 1).
//!    - `used >= slice` and the key is near its limit (`limit - anchor_spend -
//!      used <= max(limit / 20, 0)`, the 5% band the TTL cache uses) or the
//!      anchor is stale (older than twice `max_age`, so readings are not
//!      arriving): [`Decision::Throttle`] (the caller answers 429) and a
//!      background refresh is started if one is allowed.
//!    - `used >= slice` otherwise: [`Decision::Admit`] and a background
//!      refresh is started if one is allowed.
//!    - `used >= max(slice / 2, 1)`, or the anchor is older than `max_age`:
//!      [`Decision::Admit`] and a background refresh is started.
//!    - otherwise [`Decision::Admit`] with no database call at all.
//!
//! A background refresh runs the same timeout-bounded read in a spawned task;
//! requests never wait for it. There is at most one per key at a time, and a
//! new attempt may start only [`MIN_REFRESH_INTERVAL`] after the previous
//! attempt started (the blocking first read is not an attempt). On success it
//! replaces the anchor, and `used` becomes the costs recorded since that
//! read started. On failure or timeout the old anchor and `used` stay, the
//! refresh metric is counted with `result=error` or `result=timeout`, and the
//! next attempt waits the interval. A failed refresh never becomes an error
//! for a request: with an anchor present the worst case is that requests are
//! throttled once the slice is used and the key is near its limit or the
//! anchor has gone stale.
//!
//! The limit is an argument of every check, so a limit that changes between
//! requests only changes the arithmetic.
//!
//! # Overshoot bound
//!
//! - Far from the limit, a process keeps admitting past its slice while
//!   readings keep arriving (a refresh is attempted at most every
//!   [`MIN_REFRESH_INTERVAL`]), so there the bound is what the key spends
//!   between two readings, not the slice.
//! - Within 5% of the limit, or when the reading is older than twice the max
//!   age, a process stops at one slice (`share_percent` of the headroom at
//!   the last reading) past its last reading.
//! - A request is counted in `used` when its cost is recorded (see
//!   [`super::recorder`]), not while it runs, so requests in flight are never
//!   counted until then: a burst of long requests admitted together is not
//!   limited by the slice.
//! - Other processes each have their own slice of the same headroom, so keep
//!   `share_percent * processes <= 100`. Costs written by other processes, or
//!   rewritten later, are seen only at the next refresh.
//!
//! # Privacy
//!
//! Nothing is logged here. Metrics carry no tags except `result` on the
//! refresh counter, whose values are `ok`, `error` and `timeout`.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use moka::future::Cache;
use uuid::Uuid;

use super::{ApiKeySpendCache, Decision, ReadFailure, L1_CAPACITY, NEAR_LIMIT_MARGIN_DIVISOR};
use crate::metrics::consts::{
    METRIC_API_KEY_SPEND_BUDGET_ADMIT, METRIC_API_KEY_SPEND_BUDGET_OVER_LIMIT,
    METRIC_API_KEY_SPEND_BUDGET_REFRESH, METRIC_API_KEY_SPEND_BUDGET_REFRESH_MS,
    METRIC_API_KEY_SPEND_BUDGET_THROTTLE, TAG_RESULT,
};

/// An over-limit anchor at least this old is refreshed in the background.
pub const OVER_LIMIT_RECHECK: Duration = Duration::from_secs(30);
/// Least time between the starts of two refresh attempts for one key.
pub const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

const OVER_LIMIT_RECHECK_MS: u64 = OVER_LIMIT_RECHECK.as_millis() as u64;
const MIN_REFRESH_INTERVAL_MS: u64 = MIN_REFRESH_INTERVAL.as_millis() as u64;

/// Settings of the local budget (validated by the configuration).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetSettings {
    /// Share of the remaining headroom, in percent, this process may spend
    /// between readings. Kept within 1 to 50.
    pub share_percent: u64,
    /// A reading older than this starts a background refresh.
    pub max_age_secs: u64,
}

/// A database reading: its value, when its read started (monotonic clock)
/// and the process's recorded-cost counter at that moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Anchor {
    spend: i64,
    at_ms: u64,
    base: i64,
}

#[derive(Default)]
struct Slot {
    anchor: Option<Anchor>,
    refreshing: bool,
    last_attempt_ms: Option<u64>,
}

/// The state of one key. `recorded` only grows; `used` is its growth since
/// the anchor's `base`, so replacing an anchor never loses a concurrent add.
#[derive(Default)]
pub(super) struct KeyState {
    recorded: AtomicI64,
    slot: Mutex<Slot>,
}

/// The outcome of the pure rule for one check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Verdict {
    decision: Decision,
    start_refresh: bool,
}

impl KeyState {
    fn slot(&self) -> MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn add(&self, cost: i64) {
        let _ = self
            .recorded
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                Some(v.saturating_add(cost))
            });
    }

    /// Sets the anchor when there is none.
    fn install_if_empty(&self, anchor: Anchor) {
        let mut slot = self.slot();
        if slot.anchor.is_none() {
            slot.anchor = Some(anchor);
        }
    }

    /// Replaces the anchor and ends the refresh.
    fn finish_refresh(&self, anchor: Option<Anchor>) {
        let mut slot = self.slot();
        if let Some(anchor) = anchor {
            slot.anchor = Some(anchor);
        }
        slot.refreshing = false;
    }

    /// The rule. `None` when the key has no anchor yet. When the verdict
    /// asks for a refresh and one is allowed, the slot is marked as
    /// refreshing and `start_refresh` is set, so exactly one caller starts it.
    fn decide(&self, limit: i64, now_ms: u64, settings: &BudgetSettings) -> Option<Verdict> {
        let mut slot = self.slot();
        let anchor = slot.anchor?;
        let age = now_ms.saturating_sub(anchor.at_ms);
        let (decision, wants_refresh) = if anchor.spend >= limit {
            (
                Decision::OverLimit {
                    spend: anchor.spend,
                },
                age >= OVER_LIMIT_RECHECK_MS,
            )
        } else {
            let slice = slice_of(limit.saturating_sub(anchor.spend), settings.share_percent);
            let used = self
                .recorded
                .load(Ordering::Acquire)
                .saturating_sub(anchor.base);
            // The reading is "near the limit" when what is left after this
            // process's own spend since the reading is within the cache's 5%
            // band, and "stale" when readings have stopped arriving.
            let remaining = limit.saturating_sub(anchor.spend).saturating_sub(used);
            let near_limit = remaining <= (limit / NEAR_LIMIT_MARGIN_DIVISOR).max(0);
            let stale = age > settings.max_age_secs.saturating_mul(2_000);
            if used >= slice && (near_limit || stale) {
                (
                    Decision::Throttle {
                        retry_after_secs: 1,
                    },
                    true,
                )
            } else {
                (
                    Decision::Admit,
                    used >= slice
                        || used >= (slice / 2).max(1)
                        || age > settings.max_age_secs.saturating_mul(1_000),
                )
            }
        };
        let may_start = !slot.refreshing
            && slot
                .last_attempt_ms
                .is_none_or(|t| now_ms.saturating_sub(t) >= MIN_REFRESH_INTERVAL_MS);
        let start_refresh = wants_refresh && may_start;
        if start_refresh {
            slot.refreshing = true;
            slot.last_attempt_ms = Some(now_ms);
        }
        let decision = match decision {
            Decision::Throttle { .. } => Decision::Throttle {
                retry_after_secs: retry_after_secs(&slot, now_ms),
            },
            other => other,
        };
        Some(Verdict {
            decision,
            start_refresh,
        })
    }
}

/// `headroom * share_percent / 100`, at least 1, without overflow.
fn slice_of(headroom: i64, share_percent: u64) -> i64 {
    let share = i128::from(share_percent.clamp(1, 50));
    let slice = i128::from(headroom.max(0)) * share / 100;
    i64::try_from(slice).unwrap_or(i64::MAX).max(1)
}

/// Whole seconds until the next refresh can run, at least 1. While one is
/// running (or has just been started) the answer is 1.
fn retry_after_secs(slot: &Slot, now_ms: u64) -> u64 {
    if slot.refreshing {
        return 1;
    }
    let wait_ms = slot
        .last_attempt_ms
        .map_or(0, |t| (t + MIN_REFRESH_INTERVAL_MS).saturating_sub(now_ms));
    wait_ms.div_ceil(1_000).max(1)
}

/// What the shared first read hands to every concurrent first check.
#[derive(Clone)]
struct Loaded {
    state: Arc<KeyState>,
    anchor: Anchor,
}

/// Ends a refresh even if its task is dropped or panics.
struct RefreshGuard(Arc<KeyState>);

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        self.0.finish_refresh(None);
    }
}

pub(super) struct Budget {
    settings: BudgetSettings,
    states: Cache<Uuid, Arc<KeyState>>,
    /// Coalesces the blocking first read of a key. An entry lives only for
    /// the moment between the read finishing and its callers installing it.
    first_loads: Cache<Uuid, Loaded>,
}

impl Budget {
    pub(super) fn new(settings: BudgetSettings) -> Self {
        let settings = BudgetSettings {
            share_percent: settings.share_percent.clamp(1, 50),
            max_age_secs: settings.max_age_secs,
        };
        // A key that sends and records nothing for `max_age` loses its
        // state, so its next request reads the database first.
        let idle = Duration::from_secs(settings.max_age_secs.max(1));
        Self {
            settings,
            states: Cache::builder()
                .max_capacity(L1_CAPACITY)
                .time_to_idle(idle)
                .build(),
            first_loads: Cache::builder()
                .max_capacity(L1_CAPACITY)
                .time_to_live(Duration::from_secs(1))
                .build(),
        }
    }
}

impl ApiKeySpendCache {
    /// Adds a cost this process recorded for the key. Only keys that have
    /// budget state are counted; the others are read from the database when
    /// they first need a decision.
    pub async fn record_cost(&self, api_key_id: Uuid, cost: i64) {
        if cost <= 0 {
            return;
        }
        if let Some(budget) = &self.inner.budget {
            if let Some(state) = budget.states.get(&api_key_id).await {
                state.add(cost);
            }
        }
    }

    pub(super) async fn check_budget(
        &self,
        api_key_id: Uuid,
        limit: i64,
    ) -> Result<Decision, Arc<anyhow::Error>> {
        let inner = &self.inner;
        let Some(budget) = &inner.budget else {
            return Err(Arc::new(anyhow::anyhow!("local budget is not enabled")));
        };
        let mut state = budget
            .states
            .get_with(api_key_id, async { Arc::new(KeyState::default()) })
            .await;
        let mut verdict = state.decide(limit, inner.clock.now_ms(), &budget.settings);
        if verdict.is_none() {
            let loaded = self.first_read(budget, api_key_id, &state).await?;
            // Use the state `record_cost` feeds now. It is the loaded one
            // unless that was evicted during the read; then the anchor moves
            // to the current state with a zero base, which counts every cost
            // it recorded since it was created (it overcounts at worst).
            state = budget
                .states
                .get_with(api_key_id, async { Arc::new(KeyState::default()) })
                .await;
            let anchor = if Arc::ptr_eq(&state, &loaded.state) {
                loaded.anchor
            } else {
                Anchor {
                    base: 0,
                    ..loaded.anchor
                }
            };
            state.install_if_empty(anchor);
            // Only now drop the shared load: until the anchor is installed, a
            // request that finds no anchor must still find the pending load.
            budget.first_loads.invalidate(&api_key_id).await;
            verdict = state.decide(limit, inner.clock.now_ms(), &budget.settings);
        }
        let Some(verdict) = verdict else {
            // Unreachable while the anchor is installed above; fail closed.
            return Err(Arc::new(anyhow::anyhow!("API key spend has no anchor")));
        };
        if verdict.start_refresh {
            self.spawn_refresh(api_key_id, state);
        }
        let metric = match verdict.decision {
            Decision::Admit => METRIC_API_KEY_SPEND_BUDGET_ADMIT,
            Decision::Throttle { .. } => METRIC_API_KEY_SPEND_BUDGET_THROTTLE,
            Decision::OverLimit { .. } => METRIC_API_KEY_SPEND_BUDGET_OVER_LIMIT,
        };
        inner.count(metric, &[]);
        Ok(verdict.decision)
    }

    /// The blocking first read, shared by every concurrent first check and
    /// not kept when it fails.
    async fn first_read(
        &self,
        budget: &Budget,
        api_key_id: Uuid,
        state: &Arc<KeyState>,
    ) -> Result<Loaded, Arc<anyhow::Error>> {
        let inner = self.inner.clone();
        let state = state.clone();
        budget
            .first_loads
            .try_get_with(api_key_id, async move {
                let base = state.recorded.load(Ordering::Acquire);
                match inner.read_classified(api_key_id).await {
                    Ok(entry) => Ok(Loaded {
                        state,
                        anchor: Anchor {
                            spend: entry.spend,
                            at_ms: entry.read_at_ms,
                            base,
                        },
                    }),
                    Err(failure) => Err(failure.into_anyhow()),
                }
            })
            .await
    }

    /// Runs one refresh in a task; the caller has marked the slot refreshing.
    fn spawn_refresh(&self, api_key_id: Uuid, state: Arc<KeyState>) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let _guard = RefreshGuard(state.clone());
            let base = state.recorded.load(Ordering::Acquire);
            let started = Instant::now();
            let result = inner.read_classified(api_key_id).await;
            inner.metrics.record_histogram(
                METRIC_API_KEY_SPEND_BUDGET_REFRESH_MS,
                started.elapsed().as_secs_f64() * 1_000.0,
                &[],
            );
            let (label, anchor) = match result {
                Ok(entry) => (
                    "ok",
                    Some(Anchor {
                        spend: entry.spend,
                        at_ms: entry.read_at_ms,
                        base,
                    }),
                ),
                Err(ReadFailure::Timeout) => ("timeout", None),
                Err(ReadFailure::Source(_)) => ("error", None),
            };
            state.finish_refresh(anchor);
            let tag = format!("{TAG_RESULT}:{label}");
            inner.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, &[tag.as_str()]);
        });
    }

    /// Drops the key's state, as capacity or idleness would.
    #[cfg(test)]
    pub(super) async fn evict_budget_state(&self, api_key_id: Uuid) {
        if let Some(budget) = &self.inner.budget {
            budget.states.invalidate(&api_key_id).await;
        }
    }

    /// The key's anchor spend, anchor time and `used`, for tests.
    #[cfg(test)]
    pub(super) async fn budget_snapshot(&self, api_key_id: Uuid) -> Option<(i64, u64, i64)> {
        let state = self.inner.budget.as_ref()?.states.get(&api_key_id).await?;
        let anchor = state.slot().anchor?;
        let used = state
            .recorded
            .load(Ordering::Acquire)
            .saturating_sub(anchor.base);
        Some((anchor.spend, anchor.at_ms, used))
    }
}
