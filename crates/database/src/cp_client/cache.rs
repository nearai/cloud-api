//! Keyed TTL cache for cloud-platform reads: bounded, single-flight per key,
//! with a stale copy served only when the policy allows it.
//!
//! A miss or an expired entry runs the loader under that key's lock, so
//! concurrent misses for one key make one call. Callers that waited on that
//! call get its outcome, an error or an uncached answer included, rather than
//! calling again in turn; a caller arriving after it finished starts a new
//! one. A failed load is never stored. Keys can come from request input, so
//! the map is bounded: a key that does not fit is loaded without being stored.
//!
//! [`KeyedCache::invalidate`] makes every stored answer stale: the next read
//! reloads, and the old answer is kept only as the stale fallback.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Mutex as AsyncMutex;

/// Keys held at once when the cache is built with [`KeyedCache::new`].
pub const MAX_CACHE_KEYS: usize = 4096;

/// Whether an expired-or-failed reload may fall back to the last stored answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleUse {
    /// Serve the last good answer for as long as the reload keeps failing
    /// (listings: an outage must not empty the catalog).
    Forever,
    /// Serve the last good answer only while it is younger than this, measured
    /// from the fetch. Used where a stale answer decides access.
    MaxAge(Duration),
    /// Never serve an answer past its expiry (key verdicts: after the TTL the
    /// caller gets an outage, not an old verdict).
    Never,
}

impl StaleUse {
    /// Whether an answer fetched at `fetched_at` may be served at `now` while it
    /// is still within its TTL. A fresh answer is usable only while its age is at
    /// most the max age too, so the bound holds even when the TTL is longer.
    fn fresh_at(self, expires_at: Instant, fetched_at: Instant, now: Instant) -> bool {
        now < expires_at
            && match self {
                StaleUse::MaxAge(max_age) => now < fetched_at + max_age,
                StaleUse::Forever | StaleUse::Never => true,
            }
    }

    /// Whether an answer fetched at `fetched_at` may be served at `now` after a
    /// failed reload.
    fn may_serve_expired(self, fetched_at: Instant, now: Instant) -> bool {
        match self {
            StaleUse::Forever => true,
            StaleUse::MaxAge(max_age) => now < fetched_at + max_age,
            StaleUse::Never => false,
        }
    }
}

#[derive(Debug)]
struct Entry<V> {
    value: V,
    fetched_at: Instant,
    expires_at: Instant,
    /// The cache's epoch when the load started.
    epoch: u64,
}

struct SlotState<V, E> {
    entry: Option<Entry<V>>,
    /// The outcome of the last finished load, for the callers that waited on
    /// it: `Ok(stale)` means the answer is `entry`.
    last: Option<Result<bool, E>>,
}

struct SlotInner<V, E> {
    /// Loads finished in this slot. A caller that sees it change while it
    /// waits for the lock takes `last` instead of loading again.
    loads: AtomicU64,
    state: AsyncMutex<SlotState<V, E>>,
}

type Slot<V, E> = Arc<SlotInner<V, E>>;

fn new_slot<V, E>() -> Slot<V, E> {
    Arc::new(SlotInner {
        loads: AtomicU64::new(0),
        state: AsyncMutex::new(SlotState {
            entry: None,
            last: None,
        }),
    })
}

/// A value and whether it is a stale copy served after a failed reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded<V> {
    pub value: V,
    pub stale: bool,
}

pub struct KeyedCache<K, V, E> {
    slots: Mutex<HashMap<K, Slot<V, E>>>,
    /// Keys beyond `capacity`. Shared among concurrent callers while loading,
    /// and removed once idle, so they stay single-flight without being stored.
    overflow: Mutex<HashMap<K, Slot<V, E>>>,
    capacity: usize,
    stale: StaleUse,
    /// Bumped by [`Self::invalidate`]. An entry loaded in an earlier epoch is
    /// never fresh.
    epoch: AtomicU64,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl<K: Eq + Hash + Clone, V: Clone, E: Clone> KeyedCache<K, V, E> {
    pub fn new(stale: StaleUse) -> Self {
        Self::with_capacity(stale, MAX_CACHE_KEYS)
    }

    pub fn with_capacity(stale: StaleUse, capacity: usize) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            overflow: Mutex::new(HashMap::new()),
            capacity,
            stale,
            epoch: AtomicU64::new(0),
        }
    }

    /// Makes every stored answer, and any load in flight, stale: the next read
    /// of each key reloads. A stored answer still serves as the stale copy
    /// after a failed reload, as the policy allows. Callers already waiting on
    /// a load in flight still get its outcome.
    pub fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }

    /// Whether `stored` may be served as fresh at `now`.
    fn is_fresh(&self, stored: &Entry<V>, now: Instant) -> bool {
        stored.epoch == self.epoch.load(Ordering::Acquire)
            && self
                .stale
                .fresh_at(stored.expires_at, stored.fetched_at, now)
    }

    /// The value for `key`: a fresh stored one, or the result of `load`.
    /// `load` returns the value and how long it stays fresh. On a failed load
    /// the stored value is returned as stale when the policy allows it, and
    /// the error otherwise. A caller that waited on another caller's load gets
    /// that load's outcome.
    pub async fn get_or_load<F, Fut>(&self, key: K, load: F) -> Result<Loaded<V>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(V, Duration), E>>,
    {
        let guard = ReleaseOverflow {
            cache: self,
            slot: self.slot_for(&key),
            key,
        };
        self.load_into(&guard.slot, load).await
    }

    async fn load_into<F, Fut>(&self, slot: &Slot<V, E>, load: F) -> Result<Loaded<V>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(V, Duration), E>>,
    {
        let seen = slot.loads.load(Ordering::Acquire);
        let mut state = slot.state.lock().await;
        if slot.loads.load(Ordering::Acquire) != seen {
            // A load finished while this caller waited: share its outcome.
            match (&state.last, &state.entry) {
                (Some(Ok(stale)), Some(stored)) => {
                    return Ok(Loaded {
                        value: stored.value.clone(),
                        stale: *stale,
                    })
                }
                (Some(Err(error)), _) => return Err(error.clone()),
                _ => {}
            }
        }
        if let Some(stored) = state.entry.as_ref() {
            if self.is_fresh(stored, Instant::now()) {
                return Ok(Loaded {
                    value: stored.value.clone(),
                    stale: false,
                });
            }
        }
        let epoch = self.epoch.load(Ordering::Acquire);
        let outcome = match load().await {
            Ok((value, ttl)) => {
                let fetched_at = Instant::now();
                state.entry = Some(Entry {
                    value: value.clone(),
                    fetched_at,
                    expires_at: fetched_at + ttl,
                    epoch,
                });
                Ok(Loaded {
                    value,
                    stale: false,
                })
            }
            Err(error) => match state.entry.as_ref() {
                Some(stored)
                    if self
                        .stale
                        .may_serve_expired(stored.fetched_at, Instant::now()) =>
                {
                    Ok(Loaded {
                        value: stored.value.clone(),
                        stale: true,
                    })
                }
                _ => Err(error),
            },
        };
        state.last = Some(
            outcome
                .as_ref()
                .map(|loaded| loaded.stale)
                .map_err(E::clone),
        );
        slot.loads.fetch_add(1, Ordering::Release);
        outcome
    }

    /// The slot for `key`. A key already known keeps its slot. A new key is
    /// stored while there is room; past the capacity it goes to the overflow
    /// map, which still gives one shared slot per key while it is loading.
    fn slot_for(&self, key: &K) -> Slot<V, E> {
        let mut slots = locked(&self.slots);
        let mut overflow = locked(&self.overflow);
        if let Some(slot) = slots.get(key) {
            return slot.clone();
        }
        if let Some(slot) = overflow.get(key) {
            return slot.clone();
        }
        if slots.len() >= self.capacity {
            self.make_room(&mut slots);
        }
        let slot: Slot<V, E> = new_slot();
        if slots.len() < self.capacity {
            slots.insert(key.clone(), slot.clone());
        } else {
            overflow.insert(key.clone(), slot.clone());
        }
        slot
    }

    /// Makes room for one more key. Only idle slots that can no longer serve
    /// an answer are candidates: empty slots, and expired entries whose stale
    /// use has run out. Fresh entries, in-flight loads and last-good answers
    /// (under `Forever`) are never evicted. Candidates go oldest first, and
    /// only as many as needed. When none qualify, the caller uses the overflow
    /// path instead.
    fn make_room(&self, slots: &mut HashMap<K, Slot<V, E>>) {
        let now = Instant::now();
        let mut candidates: Vec<(Option<Instant>, K)> = Vec::new();
        for (key, slot) in slots.iter() {
            if Arc::strong_count(slot) > 1 {
                continue;
            }
            let Ok(state) = slot.state.try_lock() else {
                continue;
            };
            match state.entry.as_ref() {
                None => candidates.push((None, key.clone())),
                Some(stored) if self.is_fresh(stored, now) => {}
                Some(stored) if self.stale.may_serve_expired(stored.fetched_at, now) => {}
                Some(stored) => candidates.push((Some(stored.expires_at), key.clone())),
            }
        }
        candidates.sort_by_key(|(expires_at, _)| *expires_at);
        let excess = (slots.len() + 1).saturating_sub(self.capacity);
        for (_, key) in candidates.into_iter().take(excess) {
            slots.remove(&key);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        locked(&self.slots).len()
    }

    #[cfg(test)]
    fn overflow_len(&self) -> usize {
        locked(&self.overflow).len()
    }
}

/// Removes an overflow slot when its load ends, including when the load is
/// cancelled (the future is dropped). Only the last holder removes it: the
/// map and this guard are the only owners, so the count is two.
struct ReleaseOverflow<'a, K: Eq + Hash + Clone, V: Clone, E: Clone> {
    cache: &'a KeyedCache<K, V, E>,
    key: K,
    slot: Slot<V, E>,
}

impl<K: Eq + Hash + Clone, V: Clone, E: Clone> Drop for ReleaseOverflow<'_, K, V, E> {
    fn drop(&mut self) {
        let mut overflow = locked(&self.cache.overflow);
        if overflow
            .get(&self.key)
            .is_some_and(|shared| Arc::ptr_eq(shared, &self.slot) && Arc::strong_count(shared) == 2)
        {
            overflow.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn concurrent_misses_make_one_load() {
        let cache: Arc<KeyedCache<&'static str, u32, &'static str>> =
            Arc::new(KeyedCache::new(StaleUse::Forever));
        let loads = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let loads = loads.clone();
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_load("k", || async {
                        loads.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Ok((7, Duration::from_secs(30)))
                    })
                    .await
            }));
        }
        for task in tasks {
            let loaded = task.await.unwrap().unwrap();
            assert_eq!(
                loaded,
                Loaded {
                    value: 7,
                    stale: false
                }
            );
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn waiters_share_a_failed_or_uncached_load_and_later_callers_load_again() {
        for outcome in [Err("down"), Ok((7, Duration::ZERO))] {
            let cache: Arc<KeyedCache<&'static str, u32, &'static str>> =
                Arc::new(KeyedCache::new(StaleUse::Never));
            let loads = Arc::new(AtomicUsize::new(0));
            let started = Instant::now();
            let mut tasks = Vec::new();
            for _ in 0..8 {
                let cache = cache.clone();
                let loads = loads.clone();
                tasks.push(tokio::spawn(async move {
                    cache
                        .get_or_load("k", || async {
                            loads.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            outcome
                        })
                        .await
                        .map(|loaded| loaded.value)
                }));
            }
            for task in tasks {
                assert_eq!(task.await.unwrap(), outcome.map(|(value, _)| value));
            }
            assert_eq!(loads.load(Ordering::SeqCst), 1, "{outcome:?}");
            // One load's time, not eight in turn.
            assert!(
                started.elapsed() < Duration::from_millis(400),
                "{outcome:?}"
            );
            // Nothing was kept: a caller after the flight loads again.
            cache
                .get_or_load("k", || async {
                    loads.fetch_add(1, Ordering::SeqCst);
                    outcome
                })
                .await
                .ok();
            assert_eq!(loads.load(Ordering::SeqCst), 2, "{outcome:?}");
        }
    }

    #[tokio::test]
    async fn invalidate_makes_the_next_read_reload_and_keeps_the_stale_copy() {
        let cache: KeyedCache<(), u32, &'static str> = KeyedCache::new(StaleUse::Forever);
        cache
            .get_or_load((), || async { Ok((1, Duration::from_secs(30))) })
            .await
            .unwrap();
        cache.invalidate();
        let loaded = cache
            .get_or_load((), || async { Ok((2, Duration::from_secs(30))) })
            .await
            .unwrap();
        assert_eq!(loaded.value, 2);
        // Fresh again until the next invalidation, which an outage then
        // answers from the last good copy.
        let loaded = cache
            .get_or_load((), || async { Err("down") })
            .await
            .unwrap();
        assert_eq!(
            loaded,
            Loaded {
                value: 2,
                stale: false
            }
        );
        cache.invalidate();
        let loaded = cache
            .get_or_load((), || async { Err("down") })
            .await
            .unwrap();
        assert_eq!(
            loaded,
            Loaded {
                value: 2,
                stale: true
            }
        );
    }

    #[tokio::test]
    async fn forever_serves_the_last_good_copy_after_a_failed_reload() {
        let cache: KeyedCache<(), u32, &'static str> = KeyedCache::new(StaleUse::Forever);
        cache
            .get_or_load((), || async { Ok((1, Duration::ZERO)) })
            .await
            .unwrap();
        let loaded = cache
            .get_or_load((), || async { Err("down") })
            .await
            .unwrap();
        assert_eq!(
            loaded,
            Loaded {
                value: 1,
                stale: true
            }
        );
    }

    #[tokio::test]
    async fn never_refuses_a_stale_copy_once_expired() {
        let cache: KeyedCache<(), u32, &'static str> = KeyedCache::new(StaleUse::Never);
        cache
            .get_or_load((), || async { Ok((1, Duration::ZERO)) })
            .await
            .unwrap();
        let result = cache.get_or_load((), || async { Err("down") }).await;
        assert_eq!(result, Err("down"));
    }

    #[tokio::test]
    async fn a_failed_load_is_not_stored() {
        let cache: KeyedCache<(), u32, &'static str> = KeyedCache::new(StaleUse::Forever);
        assert!(cache
            .get_or_load((), || async { Err("down") })
            .await
            .is_err());
        let loaded = cache
            .get_or_load((), || async { Ok((2, Duration::from_secs(30))) })
            .await
            .unwrap();
        assert_eq!(
            loaded,
            Loaded {
                value: 2,
                stale: false
            }
        );
    }

    #[tokio::test]
    async fn keys_beyond_the_capacity_load_without_being_stored() {
        let cache: KeyedCache<u32, u32, &'static str> =
            KeyedCache::with_capacity(StaleUse::Forever, 2);
        for key in 0..5 {
            cache
                .get_or_load(key, || async { Ok((key, Duration::from_secs(30))) })
                .await
                .unwrap();
        }
        assert_eq!(cache.len(), 2);
    }

    #[tokio::test]
    async fn last_good_entries_survive_capacity_pressure() {
        let cache: KeyedCache<&'static str, u32, &'static str> =
            KeyedCache::with_capacity(StaleUse::Forever, 2);
        for (key, value) in [("a", 1), ("b", 2)] {
            // Expired at once: only the last-good copy is left.
            cache
                .get_or_load(key, || async move { Ok((value, Duration::ZERO)) })
                .await
                .unwrap();
        }
        // A third key arrives at capacity; it is loaded but not stored over them.
        cache
            .get_or_load("c", || async { Ok((3, Duration::from_secs(30))) })
            .await
            .unwrap();
        for (key, value) in [("a", 1), ("b", 2)] {
            let loaded = cache
                .get_or_load(key, || async { Err("down") })
                .await
                .unwrap();
            assert_eq!(loaded, Loaded { value, stale: true }, "{key}");
        }
    }

    #[tokio::test]
    async fn overflow_keys_stay_single_flight_and_are_released() {
        let cache: Arc<KeyedCache<&'static str, u32, &'static str>> =
            Arc::new(KeyedCache::with_capacity(StaleUse::Forever, 0));
        let loads = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let loads = loads.clone();
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_load("k", || async {
                        loads.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Ok((7, Duration::from_secs(30)))
                    })
                    .await
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap().value, 7);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        // Nothing was stored, and the idle overflow slot is gone.
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.overflow_len(), 0);
    }

    #[tokio::test]
    async fn cancelled_overflow_loads_leave_no_entries_behind() {
        let cache: Arc<KeyedCache<u32, u32, &'static str>> =
            Arc::new(KeyedCache::with_capacity(StaleUse::Forever, 0));
        let mut tasks = Vec::new();
        for key in 0..50 {
            let cache = cache.clone();
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_load(key, || async {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        Ok((key, Duration::from_secs(30)))
                    })
                    .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(cache.overflow_len(), 50);
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        assert_eq!(cache.overflow_len(), 0);
        assert_eq!(cache.len(), 0);
    }

    #[tokio::test]
    async fn fresh_entries_survive_capacity_pressure_during_an_outage() {
        let cache: KeyedCache<&'static str, u32, &'static str> =
            KeyedCache::with_capacity(StaleUse::Never, 2);
        for (key, value) in [("a", 1), ("b", 2)] {
            cache
                .get_or_load(key, || async move { Ok((value, Duration::from_secs(30))) })
                .await
                .unwrap();
        }
        // The cache is full of fresh answers, so the new key is served uncached.
        let loaded = cache
            .get_or_load("c", || async { Ok((3, Duration::from_secs(30))) })
            .await
            .unwrap();
        assert_eq!(
            loaded,
            Loaded {
                value: 3,
                stale: false
            }
        );
        assert_eq!(cache.len(), 2);
        // Outage: the earlier answers still serve, without calling the loader.
        for (key, value) in [("a", 1), ("b", 2)] {
            let loaded = cache
                .get_or_load(key, || async { Err("down") })
                .await
                .unwrap();
            assert_eq!(
                loaded,
                Loaded {
                    value,
                    stale: false
                },
                "{key}"
            );
        }
    }

    #[tokio::test]
    async fn expired_unservable_entries_make_room_oldest_first() {
        let cache: KeyedCache<&'static str, u32, &'static str> =
            KeyedCache::with_capacity(StaleUse::Never, 2);
        cache
            .get_or_load("old", || async { Ok((1, Duration::ZERO)) })
            .await
            .unwrap();
        cache
            .get_or_load("fresh", || async { Ok((2, Duration::from_secs(30))) })
            .await
            .unwrap();
        cache
            .get_or_load("new", || async { Ok((3, Duration::from_secs(30))) })
            .await
            .unwrap();
        // "old" was evicted to make room; "fresh" is kept and stored with "new".
        assert_eq!(cache.len(), 2);
        let served = cache
            .get_or_load("fresh", || async { Err("down") })
            .await
            .unwrap();
        assert_eq!(
            served,
            Loaded {
                value: 2,
                stale: false
            }
        );
    }

    #[tokio::test]
    async fn a_max_age_bound_holds_even_when_the_ttl_is_longer() {
        let cache: KeyedCache<(), u32, &'static str> =
            KeyedCache::new(StaleUse::MaxAge(Duration::from_millis(100)));
        cache
            .get_or_load((), || async { Ok((1, Duration::from_secs(60))) })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Still inside its 60 s TTL, but older than the max age: it is reloaded.
        let loaded = cache
            .get_or_load((), || async { Ok((2, Duration::from_secs(60))) })
            .await
            .unwrap();
        assert_eq!(
            loaded,
            Loaded {
                value: 2,
                stale: false
            }
        );
    }
}
