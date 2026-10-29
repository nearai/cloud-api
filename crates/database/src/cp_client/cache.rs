//! Keyed TTL cache for cloud-platform reads: bounded, single-flight per key,
//! with a stale copy served only when the policy allows it.
//!
//! A miss or an expired entry runs the loader under that key's lock, so
//! concurrent misses for one key make one call. A failed load is never
//! stored. Keys can come from request input, so the map is bounded: a key
//! that does not fit is loaded without being stored.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Mutex as AsyncMutex;

/// Keys held at once when the cache is built with [`KeyedCache::new`].
pub const MAX_CACHE_KEYS: usize = 4096;

/// Whether an expired-or-failed reload may fall back to the last stored answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleUse {
    /// Serve the last good answer for as long as the reload keeps failing
    /// (catalog reads: an outage must not empty the catalog).
    Forever,
    /// Never serve an answer past its expiry (key verdicts: after the TTL the
    /// caller gets an outage, not an old verdict).
    Never,
}

#[derive(Debug)]
struct Entry<V> {
    value: V,
    expires_at: Instant,
}

type Slot<V> = Arc<AsyncMutex<Option<Entry<V>>>>;

/// A value and whether it is a stale copy served after a failed reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded<V> {
    pub value: V,
    pub stale: bool,
}

pub struct KeyedCache<K, V> {
    slots: Mutex<HashMap<K, Slot<V>>>,
    capacity: usize,
    stale: StaleUse,
}

impl<K: Eq + Hash + Clone, V: Clone> KeyedCache<K, V> {
    pub fn new(stale: StaleUse) -> Self {
        Self::with_capacity(stale, MAX_CACHE_KEYS)
    }

    pub fn with_capacity(stale: StaleUse, capacity: usize) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            capacity,
            stale,
        }
    }

    /// The value for `key`: a fresh stored one, or the result of `load`.
    /// `load` returns the value and how long it stays fresh. On a failed load
    /// the stored value is returned as stale when the policy allows it, and
    /// the error otherwise.
    pub async fn get_or_load<E, F, Fut>(&self, key: K, load: F) -> Result<Loaded<V>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(V, Duration), E>>,
    {
        let slot = self.slot_for(key);
        let mut entry = slot.lock().await;
        if let Some(stored) = entry.as_ref() {
            if Instant::now() < stored.expires_at {
                return Ok(Loaded {
                    value: stored.value.clone(),
                    stale: false,
                });
            }
        }
        match load().await {
            Ok((value, ttl)) => {
                *entry = Some(Entry {
                    value: value.clone(),
                    expires_at: Instant::now() + ttl,
                });
                Ok(Loaded {
                    value,
                    stale: false,
                })
            }
            Err(error) => match entry.as_ref() {
                Some(stored) if self.may_serve_stale() => Ok(Loaded {
                    value: stored.value.clone(),
                    stale: true,
                }),
                _ => Err(error),
            },
        }
    }

    /// Only reached for a stored entry that has already expired (a fresh one
    /// returns before a load), so `Never` is always false here.
    fn may_serve_stale(&self) -> bool {
        match self.stale {
            StaleUse::Forever => true,
            StaleUse::Never => false,
        }
    }

    /// The slot for `key`. A key beyond the capacity gets a slot that is not
    /// stored, so it is loaded without caching.
    fn slot_for(&self, key: K) -> Slot<V> {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(slot) = slots.get(&key) {
            return slot.clone();
        }
        if slots.len() >= self.capacity {
            let now = Instant::now();
            slots.retain(|_, slot| {
                // Keep a slot in use, and any slot holding an unexpired answer.
                Arc::strong_count(slot) > 1
                    || slot
                        .try_lock()
                        .map(|entry| entry.as_ref().is_some_and(|e| now < e.expires_at))
                        .unwrap_or(true)
            });
            if slots.len() >= self.capacity {
                return Arc::new(AsyncMutex::new(None));
            }
        }
        let slot: Slot<V> = Arc::new(AsyncMutex::new(None));
        slots.insert(key, slot.clone());
        slot
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn concurrent_misses_make_one_load() {
        let cache: Arc<KeyedCache<&'static str, u32>> =
            Arc::new(KeyedCache::new(StaleUse::Forever));
        let loads = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let loads = loads.clone();
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_load::<(), _, _>("k", || async {
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
    async fn forever_serves_the_last_good_copy_after_a_failed_reload() {
        let cache: KeyedCache<(), u32> = KeyedCache::new(StaleUse::Forever);
        cache
            .get_or_load::<&str, _, _>((), || async { Ok((1, Duration::ZERO)) })
            .await
            .unwrap();
        let loaded = cache
            .get_or_load::<&str, _, _>((), || async { Err("down") })
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
        let cache: KeyedCache<(), u32> = KeyedCache::new(StaleUse::Never);
        cache
            .get_or_load::<&str, _, _>((), || async { Ok((1, Duration::ZERO)) })
            .await
            .unwrap();
        let result = cache
            .get_or_load::<&str, _, _>((), || async { Err("down") })
            .await;
        assert_eq!(result, Err("down"));
    }

    #[tokio::test]
    async fn a_failed_load_is_not_stored() {
        let cache: KeyedCache<(), u32> = KeyedCache::new(StaleUse::Forever);
        assert!(cache
            .get_or_load::<&str, _, _>((), || async { Err("down") })
            .await
            .is_err());
        let loaded = cache
            .get_or_load::<&str, _, _>((), || async { Ok((2, Duration::from_secs(30))) })
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
        let cache: KeyedCache<u32, u32> = KeyedCache::with_capacity(StaleUse::Forever, 2);
        for key in 0..5 {
            cache
                .get_or_load::<&str, _, _>(key, || async { Ok((key, Duration::from_secs(30))) })
                .await
                .unwrap();
        }
        assert_eq!(cache.len(), 2);
    }
}
