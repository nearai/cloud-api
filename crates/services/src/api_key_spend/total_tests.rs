//! The shared running total on the request path, against the in-memory store.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use uuid::Uuid;

use super::fake_store::{Call, FakeStore};
use super::ports::StoreErrorKind;
use super::tests::{FakeSource, Harness, LIMIT};
use super::*;
use crate::metrics::consts::*;

/// Polls `cond` until it holds (background tasks finish "soon").
pub(super) async fn wait_until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn on(source: Arc<FakeSource>, store: &Arc<FakeStore>) -> Harness {
    Harness::with_total(source, 10, store.clone(), TotalMode::On)
}

fn shadow(source: Arc<FakeSource>, store: &Arc<FakeStore>) -> Harness {
    Harness::with_total(source, 10, store.clone(), TotalMode::Shadow)
}

fn l1_entry(h: &Harness) -> Option<Entry> {
    h.cache.inner.l1.get(&h.id).now_or_never().flatten()
}

use futures::FutureExt;

// --- on ---

#[tokio::test]
async fn a_total_below_the_limit_avoids_the_database() {
    let store = FakeStore::new();
    let h = on(FakeSource::new(5), &store);
    store.set_total(h.id, "777");
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 777);
    assert_eq!(h.source.reads(), 0);
    assert_eq!(h.count(METRIC_API_KEY_SPEND_TOTAL_HIT, None), 1);
    assert_eq!(l1_entry(&h).map(|e| e.from_db), Some(false));
}

#[tokio::test]
async fn a_total_over_the_limit_is_confirmed_and_never_rejects_by_itself() {
    let store = FakeStore::new();
    let h = on(FakeSource::new(10), &store);
    store.set_total(h.id, &(LIMIT + 5).to_string());
    // The database says the key is far below its limit: that reading wins,
    // and the wrong total is dropped.
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 10);
    assert_eq!(h.source.reads(), 1);
    assert_eq!(h.count(METRIC_API_KEY_SPEND_TOTAL_CONFIRM, None), 1);
    wait_until("invalidate", || {
        store.count(|c| *c == Call::Invalidate) == 1
    })
    .await;
    assert_eq!(store.total(h.id), None);
    assert_eq!(h.count(METRIC_API_KEY_SPEND_TOTAL_INVALIDATE, None), 1);
}

#[tokio::test]
async fn a_total_at_the_limit_confirmed_by_the_database_rejects_without_invalidating() {
    let store = FakeStore::new();
    let h = on(FakeSource::new(LIMIT + 1), &store);
    store.set_total(h.id, &LIMIT.to_string());
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), LIMIT + 1);
    assert_eq!(h.source.reads(), 1);
    // The invariant: an in-process entry at or over the limit came from the
    // database.
    let entry = l1_entry(&h).unwrap();
    assert!(entry.spend >= LIMIT && entry.from_db);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(store.count(|c| *c == Call::Invalidate), 0);
    assert_eq!(store.total(h.id).as_deref(), Some(&*LIMIT.to_string()));
}

#[tokio::test]
async fn an_entry_from_the_total_is_confirmed_when_the_limit_is_lowered() {
    let store = FakeStore::new();
    let h = on(FakeSource::new(900), &store);
    store.set_total(h.id, "500");
    assert_eq!(h.cache.api_key_spend(h.id, 1_000).await.unwrap(), 500);
    assert_eq!(h.source.reads(), 0);
    // The limit drops to 400 while the in-process entry (500, from the total)
    // is still fresh: it must not reject without the database.
    assert_eq!(h.cache.api_key_spend(h.id, 400).await.unwrap(), 900);
    assert_eq!(h.source.reads(), 1);
    assert!(l1_entry(&h).unwrap().from_db);
}

#[tokio::test]
async fn a_missing_total_runs_begin_database_finish_and_later_reads_hit_it() {
    let store = FakeStore::new();
    let h = on(FakeSource::new(100), &store);
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 100);
    assert_eq!(h.source.reads(), 1);
    wait_until("finish", || store.total(h.id).is_some()).await;
    assert_eq!(store.total(h.id).as_deref(), Some("100"));
    assert_eq!(store.pend(h.id), None);
    assert_eq!(
        store.calls_for(h.id),
        vec![
            Call::Get,
            Call::Begin,
            Call::Finish {
                sum: 100,
                generation: "0".to_string()
            }
        ]
    );
    // Past the in-process TTL the next check reads the total, not the database.
    h.clock.advance(11_000);
    store.add_now(h.id, 25).unwrap();
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 125);
    assert_eq!(h.source.reads(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_misses_still_run_one_database_read() {
    let gate = Arc::new(Semaphore::new(0));
    let store = FakeStore::new();
    let h = on(FakeSource::gated(100, gate.clone()), &store);
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let cache = h.cache.clone();
        let id = h.id;
        tasks.push(tokio::spawn(async move {
            cache.api_key_spend(id, LIMIT).await.unwrap()
        }));
    }
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
    assert_eq!(store.count(|c| *c == Call::Get), 1);
    assert_eq!(store.count(|c| *c == Call::Begin), 1);
}

#[tokio::test]
async fn a_near_limit_reading_from_the_total_is_refreshed_after_one_second() {
    let store = FakeStore::new();
    let h = on(FakeSource::new(0), &store);
    store.set_total(h.id, "960000");
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 960_000);
    h.clock.advance(900);
    store.add_now(h.id, 10_000).unwrap();
    // Within a second the near-limit entry is used.
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 960_000);
    h.clock.advance(200);
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 970_000);
    assert_eq!(h.count(METRIC_API_KEY_SPEND_NEAR_LIMIT_REFRESH, None), 1);
    assert_eq!(h.source.reads(), 0);
}

#[tokio::test]
async fn malformed_negative_and_non_canonical_totals_are_misses() {
    let raws = [
        "abc",
        "-5",
        "007",
        "+5",
        "1.5",
        "",
        "9223372036854775808",
        " 1",
    ];
    for raw in raws {
        let store = FakeStore::new();
        let h = on(FakeSource::new(40), &store);
        store.set_total(h.id, raw);
        assert_eq!(
            h.cache.api_key_spend(h.id, LIMIT).await.unwrap(),
            40,
            "{raw:?}"
        );
        assert_eq!(h.source.reads(), 1, "{raw:?}");
        assert_eq!(
            h.count(METRIC_API_KEY_SPEND_TOTAL_MALFORMED, None),
            1,
            "{raw:?}"
        );
        assert_eq!(store.count(|c| *c == Call::Begin), 1, "{raw:?}");
    }
}

#[tokio::test]
async fn every_error_kind_falls_back_to_the_database_and_pauses_the_shared_level() {
    for kind in [
        StoreErrorKind::Timeout,
        StoreErrorKind::NoPerm,
        StoreErrorKind::Conn,
        StoreErrorKind::Other,
    ] {
        let store = FakeStore::new();
        // A 1 s TTL, shorter than the 5 s pause.
        let h = Harness::with_total(FakeSource::new(40), 1, store.clone(), TotalMode::On);
        store.set_total(h.id, "1");
        store.fail_with(Some(kind));
        assert_eq!(
            h.cache.api_key_spend(h.id, LIMIT).await.unwrap(),
            40,
            "{kind:?}"
        );
        assert_eq!(h.source.reads(), 1);
        let tag = format!("kind:{}", kind.as_str());
        assert_eq!(
            h.count(METRIC_API_KEY_SPEND_TOTAL_ERROR, Some(&tag)),
            1,
            "{kind:?}"
        );
        assert!(h.shared.as_ref().unwrap().paused());
        // While paused the store is not touched, even once the in-process
        // entry has expired.
        let calls = store.calls().len();
        h.clock.advance(2_000);
        assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 40);
        assert_eq!(h.source.reads(), 2, "{kind:?}");
        assert_eq!(store.calls().len(), calls, "{kind:?}");
        assert_eq!(h.count(METRIC_API_KEY_SPEND_TOTAL_PAUSED, None), 1);
    }
}

#[tokio::test]
async fn a_slow_store_times_out_and_falls_back() {
    let store = FakeStore::new();
    let h = on(FakeSource::new(40), &store);
    store.set_total(h.id, "1");
    store.set_delay(Duration::from_millis(400));
    let started = std::time::Instant::now();
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 40);
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "50 ms timeout"
    );
    assert_eq!(
        h.count(METRIC_API_KEY_SPEND_TOTAL_ERROR, Some("kind:timeout")),
        1
    );
    assert!(h.shared.as_ref().unwrap().paused());
}

#[tokio::test]
async fn pause_lasts_five_seconds_and_sixty_after_noperm() {
    for (kind, secs) in [
        (StoreErrorKind::Conn, 5_000u64),
        (StoreErrorKind::NoPerm, 60_000),
    ] {
        let store = FakeStore::new();
        let h = on(FakeSource::new(40), &store);
        store.fail_with(Some(kind));
        h.cache.api_key_spend(h.id, LIMIT).await.unwrap();
        let shared = h.shared.as_ref().unwrap();
        h.clock.advance(secs - 1);
        assert!(
            shared.paused(),
            "{kind:?} still paused just before {secs} ms"
        );
        h.clock.advance(1);
        assert!(!shared.paused(), "{kind:?} resumed at {secs} ms");
    }
}

#[tokio::test]
async fn the_store_is_used_again_after_the_pause() {
    let store = FakeStore::new();
    let h = on(FakeSource::new(40), &store);
    store.fail_with(Some(StoreErrorKind::Conn));
    h.cache.api_key_spend(h.id, LIMIT).await.unwrap();
    store.fail_with(None);
    store.set_total(h.id, "7");
    h.clock.advance(11_000);
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 7);
}

#[tokio::test]
async fn a_failed_database_read_is_an_error_even_with_a_working_store() {
    let store = FakeStore::new();
    let h = on(FakeSource::new(40), &store);
    *h.source.fail.lock().unwrap() = true;
    assert!(h.cache.api_key_spend(h.id, LIMIT).await.is_err());
}

// --- the seeding protocol, on the fake store ---

#[tokio::test]
async fn a_cost_added_between_begin_and_finish_is_included() {
    let store = FakeStore::new();
    let h = on(FakeSource::new(100), &store);
    let (s, id) = (store.clone(), h.id);
    // A row commits while the sum runs (after the snapshot, so the sum misses
    // it): its ADD lands in the pending costs.
    *h.source.hook.lock().unwrap() = Some(Box::new(move || {
        assert_eq!(s.add_now(id, 5), Ok(2));
    }));
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 100);
    wait_until("finish", || store.total(h.id).is_some()).await;
    assert_eq!(store.total(h.id).as_deref(), Some("105"));
    assert_eq!(store.pend(h.id), None);
}

#[tokio::test]
async fn finish_aborts_when_the_generation_changed() {
    let store = FakeStore::new();
    let id = Uuid::new_v4();
    use super::ports::SpendTotalStore;
    let generation = store.begin(id).await.unwrap();
    store.invalidate(id).await.unwrap();
    // The invalidation removed pend too; begin again so only the generation
    // is stale.
    store.begin(id).await.unwrap();
    assert_eq!(store.finish(id, 10, &generation).await, Ok(None));
    assert_eq!(store.total(id), None);
}

#[tokio::test]
async fn finish_aborts_when_pending_is_gone() {
    let store = FakeStore::new();
    let id = Uuid::new_v4();
    use super::ports::SpendTotalStore;
    let generation = store.begin(id).await.unwrap();
    store.expire_pend(id);
    assert_eq!(store.finish(id, 10, &generation).await, Ok(None));
    assert_eq!(store.total(id), None);
}

#[tokio::test]
async fn finish_aborts_when_the_total_already_exists() {
    let store = FakeStore::new();
    let id = Uuid::new_v4();
    use super::ports::SpendTotalStore;
    let generation = store.begin(id).await.unwrap();
    store.set_total(id, "50");
    assert_eq!(store.finish(id, 10, &generation).await, Ok(None));
    assert_eq!(store.total(id).as_deref(), Some("50"));
}

#[tokio::test]
async fn only_the_first_of_two_seeders_writes_the_total() {
    let store = FakeStore::new();
    let id = Uuid::new_v4();
    use super::ports::SpendTotalStore;
    let a = store.begin(id).await.unwrap();
    let b = store.begin(id).await.unwrap();
    assert_eq!(store.finish(id, 100, &a).await, Ok(Some(100)));
    assert_eq!(store.finish(id, 90, &b).await, Ok(None));
    assert_eq!(store.total(id).as_deref(), Some("100"));
}

#[tokio::test]
async fn add_never_creates_keys() {
    let store = FakeStore::new();
    let id = Uuid::new_v4();
    assert_eq!(store.add_now(id, 5), Ok(0));
    assert_eq!(store.total(id), None);
    assert_eq!(store.pend(id), None);
}

#[tokio::test]
async fn a_seed_older_than_ninety_seconds_is_not_finished() {
    for (stall_ms, finished) in [(90_000u64, true), (90_001, false)] {
        let store = FakeStore::new();
        // A long TTL, so the stalled reading is not reloaded by a second pass.
        let h = Harness::with_total(FakeSource::new(100), 300, store.clone(), TotalMode::On);
        let clock = h.clock.clone();
        // The database read takes `stall_ms` of (fake) time.
        *h.source.hook.lock().unwrap() = Some(Box::new(move || clock.advance(stall_ms)));
        assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 100);
        if finished {
            wait_until("finish", || store.total(h.id).is_some()).await;
        } else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(store.count(|c| matches!(c, Call::Finish { .. })), 0);
            assert_eq!(store.total(h.id), None);
            assert_eq!(
                h.count(METRIC_API_KEY_SPEND_TOTAL_SEED, Some("result:too_old")),
                1
            );
        }
    }
}

#[tokio::test]
async fn a_failed_begin_skips_finish_and_uses_the_database() {
    let store = FakeStore::new();
    let h = on(FakeSource::new(100), &store);
    store.fail_begin_with(Some(StoreErrorKind::Conn));
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 100);
    assert_eq!(h.source.reads(), 1);
    assert_eq!(
        h.count(METRIC_API_KEY_SPEND_TOTAL_ERROR, Some("kind:conn")),
        1
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(store.count(|c| matches!(c, Call::Finish { .. })), 0);
}

// --- shadow ---

#[tokio::test]
async fn shadow_results_equal_the_database_and_the_comparison_is_counted() {
    for (total, db, result) in [(100, 100, "equal"), (90, 100, "under"), (130, 100, "over")] {
        let store = FakeStore::new();
        let h = shadow(FakeSource::new(db), &store);
        store.set_total(h.id, &total.to_string());
        // Always the database's reading, whatever the total says.
        assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), db);
        assert_eq!(h.source.reads(), 1);
        let tag = format!("result:{result}");
        wait_until("compare", || {
            h.count(METRIC_API_KEY_SPEND_TOTAL_SHADOW, Some(&tag)) == 1
        })
        .await;
        let diffs: Vec<f64> = h
            .metrics
            .get_metrics()
            .iter()
            .filter(|m| m.name == METRIC_API_KEY_SPEND_TOTAL_SHADOW_DIFF)
            .filter_map(|m| match m.value {
                crate::metrics::capturing::MetricValue::Histogram(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(diffs, vec![(total - db).abs() as f64]);
    }
}

#[tokio::test]
async fn shadow_never_decides_from_a_total_over_the_limit() {
    let store = FakeStore::new();
    let h = shadow(FakeSource::new(5), &store);
    store.set_total(h.id, &(LIMIT * 2).to_string());
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 5);
    assert_eq!(h.count(METRIC_API_KEY_SPEND_TOTAL_HIT, None), 0);
}

#[tokio::test]
async fn shadow_seeds_a_missing_total() {
    let store = FakeStore::new();
    let h = shadow(FakeSource::new(100), &store);
    assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 100);
    wait_until("finish", || store.total(h.id).is_some()).await;
    assert_eq!(store.total(h.id).as_deref(), Some("100"));
}

#[tokio::test]
async fn a_store_failure_changes_nothing_in_shadow() {
    for kind in [
        StoreErrorKind::Timeout,
        StoreErrorKind::NoPerm,
        StoreErrorKind::Conn,
    ] {
        let store = FakeStore::new();
        let h = shadow(FakeSource::new(55), &store);
        store.set_total(h.id, "1");
        store.fail_with(Some(kind));
        assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 55);
        assert_eq!(h.source.reads(), 1);
    }
}

// --- off ---

#[tokio::test]
async fn without_a_shared_level_the_store_is_never_called() {
    let store = FakeStore::new();
    store.set_total(Uuid::nil(), "1");
    // No shared level: exactly the behaviour of the plain cache.
    let h = Harness::new(FakeSource::new(5), 10);
    for _ in 0..3 {
        h.clock.advance(11_000);
        assert_eq!(h.cache.api_key_spend(h.id, LIMIT).await.unwrap(), 5);
    }
    assert_eq!(h.source.reads(), 3);
    assert!(store.calls().is_empty());
    assert!(h.shared.is_none());
}
