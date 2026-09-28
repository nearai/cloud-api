//! Prefill-aware host scoring.
//!
//! Lower scores are better. A replica's score combines how full its running
//! queue is (`fullness`), how deep its prefill backlog is (`prefill`), and
//! how fast it's currently generating relative to the fleet (`speed`).
//! `host_score` reports the *best* (lowest-scoring) eligible replica on the
//! host (E14): cloud-api only picks the host, and the host's own
//! inference-proxy then picks which replica actually serves the request
//! using its own least-connections-plus-affinity logic, so the host is only
//! as good as its best replica.

use crate::consts::{DEFAULT_MAX_RUNNING, PREFILL_NORM_TOKENS, SPEED_FLOOR};
use crate::snapshot::{ReplicaView, RoutedCounts};

/// Requests/tokens not yet reflected in a replica's own reported load: this
/// placer's own outstanding ledger for the replica ("mine"), plus other
/// placers' routed counts observed since the replica's frame was sealed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pending {
    pub req: u32,
    pub tok: u64,
}

/// Score one replica against `pending` extra load, given the fleet's median
/// generation speed (see [`fleet_median_tps`]). Lower is better.
///
/// - `fullness = (running + queued + pending.req) / max_running.unwrap_or(DEFAULT_MAX_RUNNING)`
/// - `prefill = (prefill_backlog_tokens.unwrap_or(queued * 2000) + pending.tok) / PREFILL_NORM_TOKENS`
/// - `speed = max(SPEED_FLOOR, gen_tps.unwrap_or(fleet_median_tps) / fleet_median_tps)`
/// - `score = (fullness + prefill) / speed`
///
/// `running`/`queued` missing (`None`) are treated as 0 here; a replica with
/// *both* missing has already been excluded upstream by `Rule::Capacity`'s
/// fail-closed check, so this function never has to guess for that case.
pub fn replica_score(r: &ReplicaView, pending: Pending, fleet_median_tps: f64) -> f64 {
    let load = &r.report.load;
    let running = load.running.unwrap_or(0) as f64;
    let queued = load.queued.unwrap_or(0) as f64;

    let max_running = r
        .report
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

    let gen_tps = load.gen_tps.unwrap_or(fleet_median_tps);
    let speed = (gen_tps / fleet_median_tps).max(SPEED_FLOOR);

    (fullness + prefill) / speed
}

/// A host's score is its best (lowest) eligible replica's score (E14). A
/// host with no eligible replicas scores `f64::INFINITY`, so it is never
/// preferred over a host with at least one.
pub fn host_score(replicas: &[(&ReplicaView, Pending)], fleet_median_tps: f64) -> f64 {
    replicas
        .iter()
        .map(|(r, pending)| replica_score(r, *pending, fleet_median_tps))
        .fold(f64::INFINITY, f64::min)
}

/// The fleet's median `gen_tps`, over replicas reporting `Some(tps) > 0.0`.
/// Ties (an even sample count) use the average of the two middle values,
/// rather than the lower one, so the median doesn't favor whichever half it
/// falls in. Returns `1.0` when there are no qualifying samples, making
/// `replica_score`'s `speed` term a no-op (`gen_tps.unwrap_or(1.0) / 1.0`).
pub fn fleet_median_tps(views: &[&ReplicaView]) -> f64 {
    let mut samples: Vec<f64> = views
        .iter()
        .filter_map(|v| v.report.load.gen_tps)
        .filter(|tps| *tps > 0.0)
        .collect();
    if samples.is_empty() {
        return 1.0;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).expect("gen_tps is never NaN"));
    let n = samples.len();
    if n % 2 == 1 {
        samples[n / 2]
    } else {
        (samples[n / 2 - 1] + samples[n / 2]) / 2.0
    }
}

/// The `Pending` load for a host-level `routed` counter (E18/R3: cloud-api
/// publishes one `RoutedCounts` per host, since the host's own proxy
/// balances its own replicas).
///
/// `routed` is only trusted when it's fresher than the frame being scored
/// (`routed.since_ms > frame_reported_ms`); a `routed` counter sealed before
/// the frame can't reflect anything the frame's own load hasn't already
/// counted, so treating it as 0 avoids double counting. When trusted, the
/// result is `(routed - mine).saturating + mine` component-wise: since
/// `routed` already includes this placer's own routed requests, subtracting
/// `mine` first and adding it back avoids counting it twice while still
/// including it. Because `routed >= mine` in the steady state, this is
/// effectively `max(routed, mine)` per component when fresh, and `mine` when
/// stale or absent.
pub fn pending_for(
    routed: Option<&RoutedCounts>,
    mine: Pending,
    frame_reported_ms: u64,
) -> Pending {
    let others = match routed {
        Some(rc) if rc.since_ms > frame_reported_ms => Pending {
            req: rc.req.saturating_sub(mine.req),
            tok: rc.tok.saturating_sub(mine.tok),
        },
        _ => Pending::default(),
    };
    Pending {
        req: others.req.saturating_add(mine.req),
        tok: others.tok.saturating_add(mine.tok),
    }
}

/// Divide a host-level `Pending` evenly across `n_replicas`, using ceiling
/// division so the sum of the parts never undercounts the host total.
/// `n_replicas == 0` returns `host` unchanged (nothing to split across).
pub fn split_pending(host: Pending, n_replicas: usize) -> Pending {
    if n_replicas == 0 {
        return host;
    }
    let n = n_replicas as u64;
    Pending {
        req: (host.req as u64).div_ceil(n) as u32,
        tok: host.tok.div_ceil(n),
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
        light_running.report.load.running = Some(10);
        light_running.report.load.queued = Some(0);

        let mut heavy_backlog = light_running.clone();
        heavy_backlog.report.load.prefill_backlog_tokens = Some(40_000);

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
        slow.report.load.running = Some(20);
        slow.report.load.gen_tps = Some(180.0);

        let mut fast = view_ready();
        fast.report.load.running = Some(20);
        fast.report.load.gen_tps = Some(600.0);

        let median = fleet_median_tps(&[&slow, &fast]);
        let slow_score = replica_score(&slow, Pending::default(), median);
        let fast_score = replica_score(&fast, Pending::default(), median);
        assert!(
            slow_score > fast_score,
            "slow={slow_score} fast={fast_score}"
        );
    }

    #[test]
    fn host_score_is_best_replica() {
        // E14: the host's score tracks its best (lowest-scoring) replica.
        let mut loaded = view_ready();
        loaded.replica_id = "r1".into();
        loaded.report.load.running = Some(30);

        let mut idle = view_ready();
        idle.replica_id = "r2".into();
        idle.report.load.running = Some(0);

        let pairs = [(&loaded, Pending::default()), (&idle, Pending::default())];
        let host = host_score(&pairs, 1.0);
        let idle_alone = replica_score(&idle, Pending::default(), 1.0);
        assert!(
            (host - idle_alone).abs() < 1e-9,
            "host={host} idle_alone={idle_alone}"
        );
    }

    #[test]
    fn host_score_empty_is_infinite() {
        let pairs: [(&ReplicaView, Pending); 0] = [];
        assert_eq!(host_score(&pairs, 1.0), f64::INFINITY);
    }

    #[test]
    fn pending_does_not_double_count() {
        // E18: a routed counter sealed before the frame it's paired with
        // counts as 0, not as extra load on top of `mine`.
        let mine = Pending { req: 2, tok: 500 };
        let routed = RoutedCounts {
            req: 5,
            tok: 3_000,
            since_ms: 900,
        };
        let frame_reported_ms = 1_000; // routed.since_ms (900) is not > this
        let pending = pending_for(Some(&routed), mine, frame_reported_ms);
        assert_eq!(pending.req, mine.req);
        assert_eq!(pending.tok, mine.tok);
    }

    #[test]
    fn pending_for_uses_fresher_routed_minus_mine() {
        let mine = Pending { req: 2, tok: 500 };
        let routed = RoutedCounts {
            req: 5,
            tok: 3_000,
            since_ms: 1_500,
        };
        let frame_reported_ms = 1_000; // routed is fresher than the frame
        let pending = pending_for(Some(&routed), mine, frame_reported_ms);
        // (routed - mine) + mine == max(routed, mine) component-wise here.
        assert_eq!(pending.req, 5);
        assert_eq!(pending.tok, 3_000);
    }

    #[test]
    fn pending_for_none_routed_is_mine_only() {
        let mine = Pending { req: 3, tok: 7 };
        assert_eq!(pending_for(None, mine, 1_000).req, mine.req);
        assert_eq!(pending_for(None, mine, 1_000).tok, mine.tok);
    }

    #[test]
    fn null_backlog_uses_queued_estimate() {
        let mut v = view_ready();
        v.report.load.running = Some(0);
        v.report.load.queued = Some(3);
        v.report.load.prefill_backlog_tokens = None;
        let score = replica_score(&v, Pending::default(), 1.0);
        let expected_fullness = 3.0 / DEFAULT_MAX_RUNNING;
        let expected_prefill = (3.0 * 2_000.0) / PREFILL_NORM_TOKENS;
        let expected = expected_fullness + expected_prefill;
        assert!(
            (score - expected).abs() < 1e-9,
            "score={score} expected={expected}"
        );
    }

    #[test]
    fn split_pending_ceils() {
        let host = Pending { req: 3, tok: 101 };
        let split = split_pending(host, 2);
        assert_eq!(split.req, 2, "ceil(3/2) == 2");
        assert_eq!(split.tok, 51, "ceil(101/2) == 51");
    }

    #[test]
    fn split_pending_zero_replicas() {
        let host = Pending { req: 7, tok: 999 };
        let split = split_pending(host, 0);
        assert_eq!(split.req, host.req);
        assert_eq!(split.tok, host.tok);
    }
}
