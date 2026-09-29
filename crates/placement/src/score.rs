//! Prefill-aware replica scoring.
//!
//! Lower scores are better. A replica's score combines how full its running
//! queue is (`fullness`), how deep its prefill backlog is (`prefill`), and
//! how fast each of its streams is currently generating relative to the
//! fleet (`speed`).
//! Placement picks a replica slot, not just a host, so every slot is scored
//! on its own state and its own pending load.

use crate::consts::{DEFAULT_MAX_RUNNING, PREFILL_NORM_TOKENS, SPEED_FLOOR};
use crate::snapshot::{ReplicaView, RoutedCounts};

/// Requests/tokens not yet reflected in a replica's own reported load: the
/// fleet-wide routed counts read from Valkey, plus this placer's own ledger
/// entries that read cannot include yet (see [`pending_for`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pending {
    pub req: u32,
    pub tok: u64,
}

/// Score one replica against `pending` extra load, given the fleet's median
/// per-stream generation speed (see [`fleet_median_tps`]). Lower is better.
///
/// - `fullness = (running + queued + pending.req) / max_running.unwrap_or(DEFAULT_MAX_RUNNING)`
/// - `prefill = (prefill_backlog_tokens.unwrap_or(queued * 2000) + pending.tok) / PREFILL_NORM_TOKENS`
/// - `speed = clamp(per_stream_tps / fleet_median_tps, SPEED_FLOOR, 1 / SPEED_FLOOR)`,
///   or `1.0` when the replica's per-stream speed is unknown (see
///   [`per_stream_tps`])
/// - `score = (fullness + prefill) / speed`
///
/// `speed` is per stream because `gen_tps` is total decode throughput, which
/// grows with the running count: dividing `fullness` by it would cancel the
/// load it measures (3 streams at 195 tok/s would score like 1 at 62). A
/// per-stream collapse (decode stalled behind a prefill) still lowers
/// `speed` and raises the score.
///
/// `running`/`queued` missing (`None`) are treated as 0 here; a replica with
/// *both* missing has already been excluded upstream by `Rule::Capacity`'s
/// fail-closed check, so this function never has to guess for that case.
pub fn replica_score(r: &ReplicaView, pending: Pending, fleet_median_tps: f64) -> f64 {
    let load = &r.state.load;
    let running = load.running.unwrap_or(0) as f64;
    let queued = load.queued.unwrap_or(0) as f64;

    let max_running = r
        .state
        .limits
        .max_running
        .map(|m| m as f64)
        .unwrap_or(DEFAULT_MAX_RUNNING);
    let fullness = (running + queued + pending.req as f64) / max_running;

    let backlog = load
        .prefill_backlog_tokens
        .map(|t| t as f64)
        .unwrap_or(queued * 2_000.0);
    let prefill = (backlog + pending.tok as f64) / PREFILL_NORM_TOKENS;

    // Clamped on both sides: a self-reported huge `gen_tps` must not drive
    // the score to ~0 and win every decision.
    let speed = per_stream_tps(r).map_or(1.0, |tps| {
        (tps / fleet_median_tps).clamp(SPEED_FLOOR, 1.0 / SPEED_FLOOR)
    });

    (fullness + prefill) / speed
}

/// A replica's decode speed per running stream, `gen_tps / max(running, 1)`.
/// `None` (speed unknown) when `gen_tps` is missing, or is `0.0` with nothing
/// running: an idle replica has nothing to generate, which says nothing about
/// its speed. `0.0` with streams running is a real stall and is kept.
fn per_stream_tps(r: &ReplicaView) -> Option<f64> {
    let load = &r.state.load;
    let tps = load.gen_tps?;
    let running = load.running.unwrap_or(0);
    if tps <= 0.0 && running == 0 {
        return None;
    }
    Some(tps / f64::from(running.max(1)))
}

/// The fleet's median per-stream `gen_tps` (see [`per_stream_tps`]), over
/// replicas whose per-stream speed is known and `> 0.0`. Ties (an even
/// sample count) use the average of the two middle values, rather than the
/// lower one, so the median doesn't favor whichever half it falls in.
/// Returns `1.0` when there are no qualifying samples; every replica's speed
/// is then unknown or zero, and `replica_score` needs no real median.
pub fn fleet_median_tps(views: &[&ReplicaView]) -> f64 {
    let mut samples: Vec<f64> = views
        .iter()
        .filter_map(|v| per_stream_tps(v))
        .filter(|tps| *tps > 0.0)
        .collect();
    if samples.is_empty() {
        return 1.0;
    }
    samples.sort_by(|a, b| a.total_cmp(b));
    let n = samples.len();
    if n % 2 == 1 {
        samples[n / 2]
    } else {
        (samples[n / 2 - 1] + samples[n / 2]) / 2.0
    }
}

/// One entry of this placer's own routed ledger for a slot: the load it
/// placed there, and when Valkey acknowledged the routed-count write for it
/// (`None` while the write is still queued or in flight, or if it was
/// dropped).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnRouted {
    pub pending: Pending,
    pub acked_ms: Option<u64>,
}

/// The part of this placer's own ledger for a slot that a snapshot's routed
/// read cannot include yet, summed (saturating): every entry not
/// acknowledged by Valkey strictly before `routed_read_ms` (the snapshot's
/// [`crate::snapshot::Snapshot::routed_read_ms`]).
///
/// An entry acknowledged before the read was issued is already in `routed`,
/// so counting it again would double it. An entry acknowledged at or after
/// that instant, or not acknowledged at all, may be missing from `routed` and
/// is counted. `routed_read_ms == 0` (no read yet) counts every entry.
pub fn unseen_by_read<'a>(
    ledger: impl IntoIterator<Item = &'a OwnRouted>,
    routed_read_ms: u64,
) -> Pending {
    ledger
        .into_iter()
        .filter(|e| e.acked_ms.is_none_or(|t| t >= routed_read_ms))
        .fold(Pending::default(), |acc, e| Pending {
            req: acc.req.saturating_add(e.pending.req),
            tok: acc.tok.saturating_add(e.pending.tok),
        })
}

/// The `Pending` load for one slot's `routed` counter (cloud-api publishes
/// one `RoutedCounts` per slot, since it picks the replica).
///
/// `routed` is the fleet-wide routed count for the slot over the reader's
/// sliding window (the `{now-1s, now}` routed hashes), read from Valkey, so it
/// already includes this placer's own writes that Valkey acknowledged before
/// the read. `mine_unseen` must be only the rest of this placer's ledger for
/// the slot: the entries that read cannot include, as computed by
/// [`unseen_by_read`] against the same snapshot's `routed_read_ms`. The
/// result is `routed + mine_unseen` component-wise (saturating).
///
/// Passing the whole ledger instead double-counts this node's own placements
/// once they are flushed and read back (about one reader cycle), which
/// inflates pending on exactly the replicas this node just used. There is
/// deliberately no freshness gate against the frame's `reported_at_ms`:
/// frames arrive every 500 ms, so such a gate would discard other nodes' load
/// almost always.
pub fn pending_for(routed: Option<&RoutedCounts>, mine_unseen: Pending) -> Pending {
    match routed {
        Some(rc) => Pending {
            req: rc.req.saturating_add(mine_unseen.req),
            tok: rc.tok.saturating_add(mine_unseen.tok),
        },
        None => mine_unseen,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::view_ready;

    #[test]
    fn idle_replica_scores_near_zero() {
        let v = view_ready(); // running: Some(0), queued: Some(0), no backlog
        let score = replica_score(&v, Pending::default(), 1.0);
        assert!(score.abs() < 1e-9, "expected ~0, got {score}");
    }

    #[test]
    fn prefill_backlog_dominates() {
        // 40k backlog token vs. 10 running (out of a default 40 max_running):
        // fullness only moves the score by 0.25, prefill by 2.5.
        let mut light_running = view_ready();
        light_running.state.load.running = Some(10);
        light_running.state.load.queued = Some(0);

        let mut heavy_backlog = light_running.clone();
        heavy_backlog.state.load.prefill_backlog_tokens = Some(40_000);

        let low = replica_score(&light_running, Pending::default(), 1.0);
        let high = replica_score(&heavy_backlog, Pending::default(), 1.0);
        assert!((low - 0.25).abs() < 1e-9, "low={low}");
        assert!((high - 2.75).abs() < 1e-9, "high={high}");
        assert!(
            high - low > 2.0,
            "prefill backlog should dominate the score gap: low={low} high={high}"
        );
    }

    #[test]
    fn slow_host_scores_worse() {
        // E13: identical load, gen_tps 180 vs 600 — the slower replica must
        // score worse (higher) under the shared fleet median.
        let mut slow = view_ready();
        slow.state.load.running = Some(20);
        slow.state.load.gen_tps = Some(180.0);

        let mut fast = view_ready();
        fast.state.load.running = Some(20);
        fast.state.load.gen_tps = Some(600.0);

        let median = fleet_median_tps(&[&slow, &fast]);
        let slow_score = replica_score(&slow, Pending::default(), median);
        let fast_score = replica_score(&fast, Pending::default(), median);
        assert!(
            slow_score > fast_score,
            "slow={slow_score} fast={fast_score}"
        );
    }

    /// A gpu03-style replica: `running` streams generating `gen_tps` in
    /// total, `max_running` 32, no queue or backlog.
    fn gpu03(replica: u32, running: u32, gen_tps: Option<f64>) -> ReplicaView {
        let mut v = crate::testkit::view(crate::testkit::TEST_HOST, replica);
        v.state.limits.max_running = Some(32);
        v.state.load.running = Some(running);
        v.state.load.gen_tps = gen_tps;
        v
    }

    #[test]
    fn speed_is_per_stream_so_busier_replica_scores_worse() {
        // gpu03 snapshot A: r0 generates 195.6 tok/s across 3 streams (65 per
        // stream), r1 62.0 across 1 (62 per stream). Same hardware and the
        // same per-stream speed, so the replica carrying 3x the load must
        // score worse; total throughput must not cancel the load.
        let r0 = gpu03(0, 3, Some(195.6));
        let r1 = gpu03(1, 1, Some(62.0));
        let median = fleet_median_tps(&[&r0, &r1]);
        let busy = replica_score(&r0, Pending::default(), median);
        let light = replica_score(&r1, Pending::default(), median);
        assert!(busy > light, "busy={busy} light={light}");
    }

    #[test]
    fn zero_tps_idle_replica_is_not_penalised() {
        // An idle replica reporting gen_tps 0.0 (nothing to generate) is
        // "speed unknown", exactly like a missing sample: factor 1.0, never
        // the floor (which would multiply its score by 5).
        let busy = gpu03(0, 10, Some(500.0));
        let zero = gpu03(1, 0, Some(0.0));
        let unknown = gpu03(1, 0, None);
        let median = fleet_median_tps(&[&busy, &zero]);
        let pending = Pending { req: 1, tok: 8_000 };
        let expected = 1.0 / 32.0 + 8_000.0 / PREFILL_NORM_TOKENS;
        let z = replica_score(&zero, pending, median);
        let u = replica_score(&unknown, pending, median);
        assert!((z - expected).abs() < 1e-9, "zero-tps score={z}");
        assert!((u - expected).abs() < 1e-9, "unknown-tps score={u}");
    }

    #[test]
    fn pending_never_drops_disjoint_load() {
        // 10 requests from other nodes plus 1 local write Valkey hasn't seen
        // yet must count as at least 11: `mine` is the unseen share only, so
        // it is disjoint from `routed` and the two are summed.
        let mine = Pending { req: 1, tok: 500 };
        let routed = RoutedCounts {
            req: 10,
            tok: 300,
            since_ms: 0,
        };
        let pending = pending_for(Some(&routed), mine);
        assert_eq!(pending, Pending { req: 11, tok: 800 });
    }

    #[test]
    fn pending_for_ignores_routed_window_start() {
        // Other nodes' routed load counts regardless of `since_ms`: frames
        // arrive every 500 ms, so a gate against the frame's reported_at_ms
        // would practically never open.
        let routed = RoutedCounts {
            req: 4,
            tok: 1_000,
            since_ms: 1,
        };
        let pending = pending_for(Some(&routed), Pending::default());
        assert_eq!(pending, Pending { req: 4, tok: 1_000 });
    }

    #[test]
    fn own_placements_are_not_double_counted_after_read() {
        // Two of this node's placements were acknowledged by Valkey before the
        // snapshot's routed read was issued, so `routed` (2 requests) already
        // holds them. They must not be added again.
        let read_ms = 10_000;
        let ledger = [
            OwnRouted {
                pending: Pending { req: 1, tok: 200 },
                acked_ms: Some(9_000),
            },
            OwnRouted {
                pending: Pending { req: 1, tok: 300 },
                acked_ms: Some(read_ms - 1),
            },
        ];
        let routed = RoutedCounts {
            req: 2,
            tok: 500,
            since_ms: read_ms - 1_000,
        };
        let mine = unseen_by_read(&ledger, read_ms);
        assert_eq!(mine, Pending::default());
        assert_eq!(
            pending_for(Some(&routed), mine),
            Pending { req: 2, tok: 500 }
        );
    }

    #[test]
    fn own_placements_after_read_are_added() {
        // One placement acknowledged before the read (already in `routed`),
        // one acknowledged at/after the read was issued (the read may have
        // missed it), and one whose write has not been acknowledged yet. The
        // last two are added on top of `routed`.
        let read_ms = 10_000;
        let ledger = [
            OwnRouted {
                pending: Pending { req: 1, tok: 100 },
                acked_ms: Some(read_ms - 1),
            },
            OwnRouted {
                pending: Pending { req: 1, tok: 20 },
                acked_ms: Some(read_ms),
            },
            OwnRouted {
                pending: Pending { req: 1, tok: 3 },
                acked_ms: None,
            },
        ];
        let routed = RoutedCounts {
            req: 1,
            tok: 100,
            since_ms: read_ms - 1_000,
        };
        let mine = unseen_by_read(&ledger, read_ms);
        assert_eq!(mine, Pending { req: 2, tok: 23 });
        assert_eq!(
            pending_for(Some(&routed), mine),
            Pending { req: 3, tok: 123 }
        );
    }

    #[test]
    fn unknown_read_time_counts_every_own_placement() {
        // `routed_read_ms == 0` (never read): nothing can be assumed visible.
        let ledger = [OwnRouted {
            pending: Pending { req: 1, tok: 5 },
            acked_ms: Some(1),
        }];
        assert_eq!(unseen_by_read(&ledger, 0), Pending { req: 1, tok: 5 });
    }

    #[test]
    fn pending_for_none_routed_is_mine_only() {
        let mine = Pending { req: 3, tok: 7 };
        assert_eq!(pending_for(None, mine), mine);
    }

    #[test]
    fn speed_is_clamped_above() {
        // A self-reported absurd gen_tps can't drive the score to ~0.
        let mut honest = view_ready();
        honest.state.load.running = Some(20);
        honest.state.load.gen_tps = Some(100.0);
        let mut liar = honest.clone();
        liar.state.load.gen_tps = Some(1.0e12);

        // The median is per stream now: 100 tok/s over 20 streams is 5.0,
        // so the honest replica sits at speed 1.0.
        let base = replica_score(&honest, Pending::default(), 5.0);
        let score = replica_score(&liar, Pending::default(), 5.0);
        let clamped = base * SPEED_FLOOR;
        assert!(score > 0.0);
        assert!(
            (score - clamped).abs() < 1e-9,
            "score={score} expected clamp at {clamped}"
        );
    }

    #[test]
    fn null_backlog_uses_queued_estimate() {
        let mut v = view_ready();
        v.state.load.running = Some(0);
        v.state.load.queued = Some(3);
        v.state.load.prefill_backlog_tokens = None;
        let score = replica_score(&v, Pending::default(), 1.0);
        let expected_fullness = 3.0 / DEFAULT_MAX_RUNNING;
        let expected_prefill = (3.0 * 2_000.0) / PREFILL_NORM_TOKENS;
        let expected = expected_fullness + expected_prefill;
        assert!(
            (score - expected).abs() < 1e-9,
            "score={score} expected={expected}"
        );
    }
}
