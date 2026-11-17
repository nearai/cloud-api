//! Per-instance (organization, model) concurrent-request slots.
//!
//! Counters never expire or get evicted while a request holds a slot; an entry
//! disappears only when its last holder is released. Memory stays bounded because
//! idle keys are removed on release. Long-held slots are reported by `sweep`
//! instead of being silently reset.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::metrics::consts::{
    get_environment, METRIC_CONCURRENT_SLOTS_IN_USE, METRIC_CONCURRENT_SLOTS_MAX_PER_KEY,
    METRIC_CONCURRENT_SLOTS_OVER_THRESHOLD, TAG_ENVIRONMENT,
};
use crate::metrics::MetricsServiceTrait;

/// (organization_id, model_id)
pub(crate) type SlotKey = (Uuid, Uuid);

/// How often the leak monitor sweeps the registry.
pub(crate) const SLOT_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Generous: production streams are long (p90 ~= 274 s, max ~= 880 s observed), so 30 min is a
/// real leak, not a slow request.
pub(crate) const SLOT_HELD_WARN_THRESHOLD: Duration = Duration::from_secs(30 * 60);

/// Registry of live concurrency slots, keyed by (organization, model).
///
/// The in-flight count for a key is simply `holders.len()` under a single
/// `Mutex`, so acquire, release and key removal are trivially atomic with
/// respect to each other: two entries for the same key can never coexist, and a
/// key with a live holder can never be removed (it is non-empty).
pub(crate) struct ConcurrencySlots {
    keys: Mutex<HashMap<SlotKey, KeyState>>,
    next_slot_id: AtomicU64,
}

#[derive(Default)]
struct KeyState {
    /// In-flight count for the key == `holders.len()`.
    holders: HashMap<u64, Holder>,
}

struct Holder {
    acquired_at: Instant,
    /// Set once `sweep` has reported this holder as held over the threshold, so
    /// each leaked/long slot is logged once, not every tick.
    reported: bool,
}

/// One admitted request. Releasing is `Drop`: not `Clone`, so release happens exactly once.
pub(crate) struct ConcurrencySlot {
    registry: Arc<ConcurrencySlots>,
    key: SlotKey,
    id: u64,
}

impl std::fmt::Debug for ConcurrencySlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConcurrencySlot")
            .field("organization_id", &self.key.0)
            .field("model_id", &self.key.1)
            .field("slot_id", &self.id)
            .finish()
    }
}

impl Drop for ConcurrencySlot {
    fn drop(&mut self) {
        self.registry.release(self.key, self.id);
    }
}

/// A slot that has been held longer than the leak threshold, reported once.
pub(crate) struct LongHeldSlot {
    pub organization_id: Uuid,
    pub model_id: Uuid,
    pub held_for: Duration,
}

/// Snapshot of the registry taken by [`ConcurrencySlots::sweep`].
#[derive(Default)]
pub(crate) struct SweepReport {
    /// Total holders across all keys.
    pub in_use: usize,
    /// Number of keys with at least one holder.
    pub tracked_keys: usize,
    /// Largest `holders.len()` of any key (0 when the registry is empty).
    pub max_per_key: usize,
    pub busiest_key: Option<SlotKey>,
    /// Holders (reported or not) held longer than the threshold.
    pub over_threshold: usize,
    /// Holders crossing the threshold for the first time; marks them reported.
    pub newly_over_threshold: Vec<LongHeldSlot>,
}

impl ConcurrencySlots {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            keys: Mutex::new(HashMap::new()),
            next_slot_id: AtomicU64::new(0),
        })
    }

    /// A poisoned mutex must never panic here: `Drop` runs during unwinding.
    fn lock(&self) -> MutexGuard<'_, HashMap<SlotKey, KeyState>> {
        self.keys.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Admit the request when the key has fewer than `limit` holders.
    ///
    /// Returns `Err(current_count)` when the key is at or over the limit. A
    /// rejection never inserts anything, so probing an idle key cannot grow the
    /// registry.
    pub(crate) fn try_acquire(
        self: &Arc<Self>,
        key: SlotKey,
        limit: u32,
    ) -> Result<ConcurrencySlot, u32> {
        let mut keys = self.lock();
        let current = keys.get(&key).map_or(0, |state| state.holders.len());
        let current = u32::try_from(current).unwrap_or(u32::MAX);
        if current >= limit {
            return Err(current);
        }

        let id = self.next_slot_id.fetch_add(1, Ordering::Relaxed);
        keys.entry(key).or_default().holders.insert(
            id,
            Holder {
                acquired_at: Instant::now(),
                reported: false,
            },
        );
        drop(keys);

        Ok(ConcurrencySlot {
            registry: Arc::clone(self),
            key,
            id,
        })
    }

    /// Release one holder, removing the key once its last holder is gone.
    fn release(&self, key: SlotKey, id: u64) {
        let mut keys = self.lock();
        if let Some(state) = keys.get_mut(&key) {
            state.holders.remove(&id);
            if state.holders.is_empty() {
                keys.remove(&key);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn in_use(&self) -> usize {
        self.lock().values().map(|state| state.holders.len()).sum()
    }

    /// In-flight count for one key; 0 when the key is not tracked.
    #[cfg(test)]
    pub(crate) fn in_use_for(&self, key: SlotKey) -> usize {
        self.lock().get(&key).map_or(0, |state| state.holders.len())
    }

    #[cfg(test)]
    pub(crate) fn tracked_keys(&self) -> usize {
        self.lock().len()
    }

    /// Snapshot the registry and mark newly long-held slots as reported.
    pub(crate) fn sweep(&self, now: Instant, threshold: Duration) -> SweepReport {
        let mut report = SweepReport::default();
        let mut keys = self.lock();

        for (key, state) in keys.iter_mut() {
            let count = state.holders.len();
            report.in_use += count;
            if count > report.max_per_key {
                report.max_per_key = count;
                report.busiest_key = Some(*key);
            }

            for holder in state.holders.values_mut() {
                let held_for = now.saturating_duration_since(holder.acquired_at);
                if held_for >= threshold {
                    report.over_threshold += 1;
                    if !holder.reported {
                        holder.reported = true;
                        report.newly_over_threshold.push(LongHeldSlot {
                            organization_id: key.0,
                            model_id: key.1,
                            held_for,
                        });
                    }
                }
            }
        }

        report.tracked_keys = keys.len();
        report
    }
}

/// Spawn the periodic leak sweep. Holds only a `Weak` to the registry so the
/// task ends when the service is dropped (tests build many services).
pub(crate) fn spawn_slot_monitor(
    registry: &Arc<ConcurrencySlots>,
    metrics: Arc<dyn MetricsServiceTrait>,
) {
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle,
        Err(_) => {
            tracing::debug!("No Tokio runtime available; concurrency slot monitor not started");
            return;
        }
    };

    let weak = Arc::downgrade(registry);
    std::mem::drop(handle.spawn(async move {
        let tags = [format!("{TAG_ENVIRONMENT}:{}", get_environment())];
        let tags_str: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();

        let mut ticker = tokio::time::interval(SLOT_SWEEP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick resolves immediately; skip it so the first sweep runs
        // one interval after startup.
        ticker.tick().await;

        loop {
            ticker.tick().await;

            let report = {
                let Some(registry) = weak.upgrade() else {
                    tracing::debug!("Concurrency slot registry dropped; stopping slot monitor");
                    return;
                };
                registry.sweep(Instant::now(), SLOT_HELD_WARN_THRESHOLD)
            };

            metrics.record_histogram(
                METRIC_CONCURRENT_SLOTS_IN_USE,
                report.in_use as f64,
                &tags_str,
            );
            metrics.record_histogram(
                METRIC_CONCURRENT_SLOTS_MAX_PER_KEY,
                report.max_per_key as f64,
                &tags_str,
            );
            metrics.record_histogram(
                METRIC_CONCURRENT_SLOTS_OVER_THRESHOLD,
                report.over_threshold as f64,
                &tags_str,
            );

            for slot in &report.newly_over_threshold {
                tracing::warn!(
                    organization_id = %slot.organization_id,
                    model_id = %slot.model_id,
                    held_secs = slot.held_for.as_secs(),
                    "Concurrent request slot held longer than the leak threshold"
                );
            }

            let (busiest_organization_id, busiest_model_id) = report
                .busiest_key
                .map_or((None, None), |(org_id, model_id)| {
                    (Some(org_id), Some(model_id))
                });
            tracing::debug!(
                in_use = report.in_use,
                tracked_keys = report.tracked_keys,
                max_per_key = report.max_per_key,
                over_threshold = report.over_threshold,
                organization_id = ?busiest_organization_id,
                model_id = ?busiest_model_id,
                "Concurrency slot sweep"
            );
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::capturing::{CapturingMetricsService, MetricValue};
    use std::sync::atomic::AtomicU32;

    fn test_key() -> SlotKey {
        (Uuid::new_v4(), Uuid::new_v4())
    }

    /// The old moka cache expired the counter 600 s after insertion regardless of
    /// in-flight holders, which let an organization exceed its cap while the old
    /// requests were still streaming. The registry has no TTL, so the limit holds
    /// for as long as the requests do.
    #[tokio::test(start_paused = true)]
    async fn limit_holds_for_requests_held_past_600s_and_900s() {
        let registry = ConcurrencySlots::new();
        let key = test_key();
        let limit = 3;

        let mut slots = Vec::new();
        for _ in 0..limit {
            slots.push(
                registry
                    .try_acquire(key, limit)
                    .expect("acquire under the limit should be admitted"),
            );
        }
        assert_eq!(registry.in_use_for(key), limit as usize);

        // Past the old 600 s TTL.
        tokio::time::advance(Duration::from_secs(601)).await;
        assert_eq!(
            registry.try_acquire(key, limit).map(|_| ()),
            Err(limit),
            "the limit must still hold after the old TTL would have expired"
        );

        // And well past it.
        tokio::time::advance(Duration::from_secs(400)).await;
        assert_eq!(
            registry.try_acquire(key, limit).map(|_| ()),
            Err(limit),
            "the limit must still hold after 1000 s"
        );

        // Releasing one slot frees exactly one.
        slots.pop();
        assert_eq!(registry.in_use_for(key), (limit - 1) as usize);
        let extra = registry
            .try_acquire(key, limit)
            .expect("one released slot admits exactly one request");
        assert_eq!(
            registry.try_acquire(key, limit).map(|_| ()),
            Err(limit),
            "no second request may slip through"
        );
        drop(extra);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stress_never_exceeds_limit_and_ends_at_zero() {
        const LIMIT: u32 = 8;
        const TASKS: usize = 64;
        const ITERATIONS: usize = 50;

        let registry = ConcurrencySlots::new();
        let key = test_key();
        let in_flight = Arc::new(AtomicU32::new(0));
        let max_observed = Arc::new(AtomicU32::new(0));
        let admitted = Arc::new(AtomicU64::new(0));
        let rejected = Arc::new(AtomicU64::new(0));

        let mut handles = Vec::with_capacity(TASKS);
        for _ in 0..TASKS {
            let registry = Arc::clone(&registry);
            let in_flight = Arc::clone(&in_flight);
            let max_observed = Arc::clone(&max_observed);
            let admitted = Arc::clone(&admitted);
            let rejected = Arc::clone(&rejected);
            handles.push(tokio::spawn(async move {
                for _ in 0..ITERATIONS {
                    match registry.try_acquire(key, LIMIT) {
                        Ok(slot) => {
                            admitted.fetch_add(1, Ordering::Relaxed);
                            let observed = in_flight.fetch_add(1, Ordering::AcqRel) + 1;
                            max_observed.fetch_max(observed, Ordering::AcqRel);
                            tokio::task::yield_now().await;
                            in_flight.fetch_sub(1, Ordering::AcqRel);
                            drop(slot);
                        }
                        Err(_) => {
                            rejected.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }
        for handle in handles {
            handle.await.expect("worker task should not panic");
        }

        let admitted = admitted.load(Ordering::Relaxed);
        let rejected = rejected.load(Ordering::Relaxed);
        assert!(
            max_observed.load(Ordering::Relaxed) <= LIMIT,
            "observed {} concurrent holders for a limit of {LIMIT}",
            max_observed.load(Ordering::Relaxed)
        );
        assert_eq!(
            admitted + rejected,
            (TASKS * ITERATIONS) as u64,
            "every attempt must be either admitted or rejected"
        );
        assert!(
            admitted >= u64::from(LIMIT),
            "expected at least {LIMIT} admissions, got {admitted}"
        );
        assert_eq!(registry.in_use(), 0, "every slot must be released");
        assert_eq!(registry.tracked_keys(), 0, "idle keys must be reclaimed");
    }

    #[tokio::test]
    async fn idle_key_is_reclaimed_and_restarts_from_zero_without_touching_other_keys() {
        let registry = ConcurrencySlots::new();
        let key_a = test_key();
        let key_b = test_key();

        {
            let _slot = registry
                .try_acquire(key_a, 1)
                .expect("A should be admitted");
            assert_eq!(registry.tracked_keys(), 1);
        }
        assert_eq!(
            registry.tracked_keys(),
            0,
            "an idle key must be reclaimed on release"
        );

        let slot_b = registry
            .try_acquire(key_b, 1)
            .expect("B should be admitted");

        // A restarts from zero without disturbing B.
        let slot_a = registry
            .try_acquire(key_a, 1)
            .expect("A should be admitted again");
        assert_eq!(registry.in_use_for(key_a), 1);
        assert_eq!(registry.in_use_for(key_b), 1);
        assert_eq!(registry.tracked_keys(), 2);

        // A rejection must not create or keep an entry.
        assert_eq!(registry.try_acquire(key_a, 1).map(|_| ()), Err(1));
        assert_eq!(registry.tracked_keys(), 2);

        let untracked = test_key();
        assert_eq!(registry.try_acquire(untracked, 0).map(|_| ()), Err(0));
        assert_eq!(
            registry.tracked_keys(),
            2,
            "a rejected acquire must not insert a key"
        );
        assert_eq!(registry.in_use_for(untracked), 0);

        drop(slot_a);
        drop(slot_b);
        assert_eq!(registry.in_use(), 0);
        assert_eq!(registry.tracked_keys(), 0);
    }

    /// Ported from the moka-cache era: keys are independent per organization and
    /// per model.
    #[tokio::test]
    async fn test_concurrent_limit_different_orgs_and_models_independent() {
        let registry = ConcurrencySlots::new();
        let org1 = Uuid::new_v4();
        let org2 = Uuid::new_v4();
        let model_a = Uuid::new_v4();
        let model_b = Uuid::new_v4();
        let limit = 2;

        // Fill up org1 + model_a.
        let mut org1_model_a = Vec::new();
        for _ in 0..limit {
            org1_model_a.push(
                registry
                    .try_acquire((org1, model_a), limit)
                    .expect("org1+model_a should be admitted under its own limit"),
            );
        }
        assert_eq!(
            registry.try_acquire((org1, model_a), limit).map(|_| ()),
            Err(limit)
        );

        // Different model, same org: unaffected.
        let _org1_model_b = registry
            .try_acquire((org1, model_b), limit)
            .expect("org1+model_b should start from zero");
        assert_eq!(registry.in_use_for((org1, model_b)), 1);

        // Different org, same model: unaffected.
        let _org2_model_a = registry
            .try_acquire((org2, model_a), limit)
            .expect("org2+model_a should start from zero");
        assert_eq!(registry.in_use_for((org2, model_a)), 1);
        assert_eq!(registry.in_use_for((org1, model_a)), limit as usize);
    }

    /// `sweep` takes `now` as a parameter, so the test moves the clock by
    /// passing a later instant (the registry reads `std::time::Instant`, which
    /// `tokio::time::advance` does not affect).
    #[test]
    fn sweep_reports_long_held_slots_once() {
        let registry = ConcurrencySlots::new();
        let key = test_key();
        let started = Instant::now();
        let slot = registry.try_acquire(key, 3).expect("should be admitted");

        let report = registry.sweep(started, SLOT_HELD_WARN_THRESHOLD);
        assert_eq!(report.in_use, 1);
        assert_eq!(report.tracked_keys, 1);
        assert_eq!(report.max_per_key, 1);
        assert_eq!(report.busiest_key, Some(key));
        assert_eq!(report.over_threshold, 0);
        assert!(report.newly_over_threshold.is_empty());

        let later = started + Duration::from_secs(31 * 60);

        let report = registry.sweep(later, SLOT_HELD_WARN_THRESHOLD);
        assert_eq!(report.over_threshold, 1);
        assert_eq!(report.newly_over_threshold.len(), 1);
        let long_held = &report.newly_over_threshold[0];
        assert_eq!(long_held.organization_id, key.0);
        assert_eq!(long_held.model_id, key.1);
        assert!(long_held.held_for >= SLOT_HELD_WARN_THRESHOLD);

        // Reported once, still counted.
        let report = registry.sweep(later, SLOT_HELD_WARN_THRESHOLD);
        assert_eq!(report.over_threshold, 1);
        assert!(
            report.newly_over_threshold.is_empty(),
            "a long-held slot must only be logged once"
        );

        drop(slot);
        let report = registry.sweep(later, SLOT_HELD_WARN_THRESHOLD);
        assert_eq!(report.in_use, 0);
        assert_eq!(report.tracked_keys, 0);
        assert_eq!(report.max_per_key, 0);
        assert_eq!(report.busiest_key, None);
        assert_eq!(report.over_threshold, 0);
        assert!(report.newly_over_threshold.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn slot_monitor_records_slot_histograms() {
        let metrics = Arc::new(CapturingMetricsService::new());
        let registry = ConcurrencySlots::new();
        spawn_slot_monitor(&registry, metrics.clone());

        let key = test_key();
        let _slot = registry.try_acquire(key, 3).expect("should be admitted");

        let mut recorded = Vec::new();
        for _ in 0..20 {
            tokio::time::advance(SLOT_SWEEP_INTERVAL).await;
            tokio::task::yield_now().await;
            recorded = metrics.get_metrics();
            if recorded.len() >= 3 {
                break;
            }
        }

        let names: Vec<&str> = recorded.iter().map(|m| m.name.as_str()).collect();
        for expected in [
            METRIC_CONCURRENT_SLOTS_IN_USE,
            METRIC_CONCURRENT_SLOTS_MAX_PER_KEY,
            METRIC_CONCURRENT_SLOTS_OVER_THRESHOLD,
        ] {
            assert!(
                names.contains(&expected),
                "expected {expected} to be recorded, got {names:?}"
            );
        }

        let in_use = recorded
            .iter()
            .find(|m| m.name == METRIC_CONCURRENT_SLOTS_IN_USE)
            .expect("in_use histogram should be recorded");
        match in_use.value {
            MetricValue::Histogram(value) => assert_eq!(value, 1.0),
            ref other => panic!("expected a histogram, got {other:?}"),
        }
        assert_eq!(
            in_use.tags,
            vec![format!("{TAG_ENVIRONMENT}:{}", get_environment())],
            "slot metrics must carry the environment tag only (no org/model cardinality)"
        );
    }
}
