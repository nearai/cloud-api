use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use super::budget::{MIN_REFRESH_INTERVAL, OVER_LIMIT_RECHECK};
use super::test_support::{Harness, Source, LIMIT, T0};
use super::*;
use crate::metrics::consts::*;

/// 25% of the headroom of an empty key.
const SLICE: i64 = 250_000;
/// A reading inside the 5% band (limit / 20 = 50,000): 60,000 of headroom, so
/// a slice of 15,000, and using it leaves 45,000.
const NEAR_SPEND: i64 = LIMIT - 60_000;
const NEAR_SLICE: i64 = 15_000;

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

#[test]
fn the_production_clock_never_goes_backwards() {
    // The clock is monotonic, so a wall-clock step cannot be expressed here;
    // this proves it is non-decreasing across calls, including after a delay.
    let clock = ports::SystemClock;
    let mut last = ports::Clock::now_ms(&clock);
    for _ in 0..1_000 {
        let now = ports::Clock::now_ms(&clock);
        assert!(now >= last, "clock went backwards: {last} -> {now}");
        last = now;
    }
    std::thread::sleep(Duration::from_millis(30));
    assert!(ports::Clock::now_ms(&clock) >= last + 30);
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
    // The only read is the first one.
    assert_eq!(
        h.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, Some("result:ok")),
        1
    );
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
    // The first read and the refresh.
    assert_eq!(
        h.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, Some("result:ok")),
        2
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
    h.record(SLICE).await;
    for _ in 0..3 {
        assert_eq!(h.check().await, Decision::Admit);
    }
    settle().await;
    assert_eq!(h.source.reads(), 2, "no second refresh while one runs");
    assert_eq!(h.count(METRIC_API_KEY_SPEND_BUDGET_THROTTLE, None), 0);
    h.source.set(2 * SLICE);
    gate.add_permits(1);
    settle().await;
    assert_eq!(h.snapshot().await.unwrap().0, 2 * SLICE);
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
    // Limit 6,000: the band is 300. Anchor 5,600 leaves 400, a slice of 100,
    // and using it leaves 300. Anchor 5,599 leaves 401, a slice of 100, and
    // using it leaves 301. The gate keeps the refreshes the checks start from
    // finishing.
    let cases = [
        (5_600, 99, false),
        (5_600, 100, true),
        (5_599, 100, false),
        (5_599, 101, true),
    ];
    for (anchor, used, throttled) in cases {
        let gate = Arc::new(Semaphore::new(1));
        let h = Harness::new(Source::gated(anchor, gate));
        assert_eq!(h.cache.check(h.id, 6_000).await.unwrap(), Decision::Admit);
        h.record(used).await;
        let decision = h.cache.check(h.id, 6_000).await.unwrap();
        if throttled {
            assert!(retry_after(decision) >= 1, "{anchor} {used}");
        } else {
            assert_eq!(decision, Decision::Admit, "{anchor} {used}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_arithmetic_does_not_overflow_or_stall() {
    // (anchor, limit, costs recorded, throttled after them)
    let cases: [(i64, i64, &[i64], bool); 5] = [
        // Huge limit, recorded costs saturate instead of wrapping.
        (0, i64::MAX, &[i64::MAX, i64::MAX], true),
        // A negative anchor under a limit of zero, with more spent than is left.
        (-5, 0, &[1_000], true),
        // The lowest anchor under the highest limit is far from it.
        (i64::MIN, i64::MAX, &[i64::MAX / 5], false),
        (i64::MIN, i64::MAX, &[], false),
        // One unit of headroom still has a slice of one unit.
        (LIMIT - 1, LIMIT, &[1], true),
    ];
    for (anchor, limit, costs, throttled) in cases {
        let h = Harness::new(Source::new(anchor));
        assert_eq!(h.cache.check(h.id, limit).await.unwrap(), Decision::Admit);
        for cost in costs {
            h.record(*cost).await;
        }
        let decision = h.cache.check(h.id, limit).await.unwrap();
        if throttled {
            assert!(retry_after(decision) >= 1, "{anchor} {limit}");
        } else {
            assert_eq!(decision, Decision::Admit, "{anchor} {limit}");
        }
    }
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
    let h = Harness::new(Source::new(0));
    assert_eq!(h.check().await, Decision::Admit);
    h.record(SLICE / 2).await;
    h.source.hang(true);
    assert_eq!(h.check().await, Decision::Admit);
    settle().await;
    // Requests are not held back while it hangs.
    assert_eq!(h.check().await, Decision::Admit);
    tokio::time::sleep(READ_TIMEOUT + Duration::from_secs(1)).await;
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

// --- read timeout, with the budget on and off ---

#[tokio::test(start_paused = true)]
async fn a_read_is_bounded_by_the_timeout_and_a_timeout_is_not_kept() {
    for budget in [Some(60), None] {
        let h = Harness::with_budget(Source::new(7), budget);
        // A read that finishes just inside the timeout succeeds.
        h.source.delay_ms(READ_TIMEOUT.as_millis() as u64 - 1);
        assert_eq!(h.check().await, Decision::Admit);
        assert_eq!(
            h.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, Some("result:timeout")),
            0
        );

        // One that never finishes fails exactly at the timeout, for the
        // first read of a key (budget on) and for every read (budget off).
        let h = Harness::with_budget(Source::new(7), budget);
        h.source.hang(true);
        let started = tokio::time::Instant::now();
        assert!(h.cache.check(h.id, LIMIT).await.is_err());
        assert_eq!(started.elapsed(), READ_TIMEOUT);
        assert!(h.snapshot().await.is_none());
        assert_eq!(
            h.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, Some("result:timeout")),
            1
        );
        // Nothing is kept: the next check reads again and succeeds.
        h.source.hang(false);
        assert_eq!(h.check().await, Decision::Admit);
        assert_eq!(h.source.reads(), 2);
    }
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
async fn a_check_that_looked_before_the_anchor_was_installed_does_not_read_again() {
    let h = Arc::new(Harness::new(Source::new(0)));
    let hold = h.cache.hold_next_first_check().unwrap();
    let late = {
        let h = h.clone();
        tokio::spawn(async move { h.check().await })
    };
    // The late check has found no anchor and waits before loading.
    hold.reached.acquire().await.unwrap().forget();
    // Another check reads, installs the anchor and drops the shared load.
    assert_eq!(h.check().await, Decision::Admit);
    assert_eq!(h.source.reads(), 1);
    hold.release.add_permits(1);
    assert_eq!(late.await.unwrap(), Decision::Admit);
    assert_eq!(
        h.source.reads(),
        1,
        "a key with an anchor is not read again"
    );
    assert_eq!(
        h.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, Some("result:ok")),
        1
    );
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
    // The limit drops to 20,500: the slice is 5,125, already used, and 500
    // is left, inside the 5% band (1,025).
    let decision = h.cache.check(h.id, 20_500).await.unwrap();
    assert!(retry_after(decision) >= 1);
    // And the anchor is over a limit of zero.
    let decision = h.cache.check(h.id, 0).await.unwrap();
    assert_eq!(decision, Decision::OverLimit { spend: 0 });
}

// --- off mode ---

#[tokio::test(start_paused = true)]
async fn with_the_budget_off_every_check_reads_and_compares_the_spend() {
    let h = Harness::off(Source::new(LIMIT - 1));
    assert_eq!(h.check().await, Decision::Admit);
    h.source.set(LIMIT);
    assert_eq!(h.check().await, Decision::OverLimit { spend: LIMIT });
    h.source.set(LIMIT + 5);
    assert_eq!(h.check().await, Decision::OverLimit { spend: LIMIT + 5 });
    // Each check was a read, whatever the clock says, and every read is counted.
    assert_eq!(h.source.reads(), 3);
    assert_eq!(
        h.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, Some("result:ok")),
        3
    );
    // A failed read is an error, not a decision; the next check reads again.
    h.source.fail(true);
    assert!(h.cache.check(h.id, LIMIT).await.is_err());
    assert_eq!(
        h.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, Some("result:error")),
        1
    );
    h.source.fail(false);
    h.source.set(0);
    assert_eq!(h.check().await, Decision::Admit);
    // Recording is a no-op, no budget state exists, no decision is counted.
    h.record(5).await;
    assert!(h.snapshot().await.is_none());
    assert_eq!(h.count(METRIC_API_KEY_SPEND_BUDGET_ADMIT, None), 0);
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
    let h = Arc::new(Harness::with_budget(
        Source::gated(0, gate.clone()),
        Some(1),
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
