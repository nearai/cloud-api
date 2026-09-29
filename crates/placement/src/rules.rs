//! Ordered eligibility rules (Chain of Responsibility), in two stages.
//!
//! Stage 1 is per replica: [`first_exclusion`] runs each [`Rule`] in
//! [`RULES`] order against a [`ReplicaView`] and returns the first one that
//! excludes it, or `None` if the replica survives. Stage 2 is `Rule::Lane`,
//! which needs a view of every live replica at once, so `Placer::place`
//! evaluates it after stage 1 (see `policy::lane_admits`). Exclusions from
//! both stages are tallied per rule in [`ALL_RULES`] order.

use crate::consts::{FRESH_MAX_MS, KV_MAX, MAX_FUTURE_SKEW_MS};
use crate::decision::PlaceInput;
use crate::frame::Lifecycle;
use crate::snapshot::ReplicaView;

/// A single eligibility check, in the order they are applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    Lifecycle,
    Freshness,
    Capacity,
    Context,
    /// Stage 2: the heavy lane. Evaluated by `Placer::place` over the
    /// stage-1 survivors, never by [`Rule::check`].
    Lane,
}

/// A replica was excluded by `Rule`. Carries no request content — safe to
/// log (see `Rule::as_str`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Exclusion(pub Rule);

/// The per-replica (stage 1) rules, in the order `first_exclusion` applies
/// them.
pub const RULES: [Rule; 4] = [
    Rule::Lifecycle,
    Rule::Freshness,
    Rule::Capacity,
    Rule::Context,
];

/// Every rule, stage 1 then stage 2: the order of `DecisionRecord::excluded`.
pub const ALL_RULES: [Rule; 5] = [
    Rule::Lifecycle,
    Rule::Freshness,
    Rule::Capacity,
    Rule::Context,
    Rule::Lane,
];

impl Rule {
    /// A stable, content-free name for logging/metrics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Rule::Lifecycle => "lifecycle",
            Rule::Freshness => "freshness",
            Rule::Capacity => "capacity",
            Rule::Context => "context",
            Rule::Lane => "lane",
        }
    }

    /// Checks `r` against this stage-1 rule. `now_ms` is the caller's clock,
    /// used by `Freshness` to judge how old `engine_sampled_at_ms` is.
    /// `Rule::Lane` always passes here: it is not a per-replica rule.
    pub fn check(self, r: &ReplicaView, input: &PlaceInput, now_ms: u64) -> Result<(), Exclusion> {
        match self {
            Rule::Lifecycle => {
                if r.state.lifecycle_state == Lifecycle::Ready {
                    Ok(())
                } else {
                    Err(Exclusion(self))
                }
            }
            Rule::Freshness => match r.state.engine_sampled_at_ms {
                // Fresh: no older than FRESH_MAX_MS and no further in the future
                // than MAX_FUTURE_SKEW_MS (a skewed clock must not look current).
                Some(t)
                    if t <= now_ms.saturating_add(MAX_FUTURE_SKEW_MS)
                        && now_ms.saturating_sub(t) <= FRESH_MAX_MS =>
                {
                    Ok(())
                }
                // `None`, too old, or too far in the future: a wedged engine
                // keeps reporting an old `t` forever, so a missing sample time
                // is stale too (E9).
                _ => Err(Exclusion(self)),
            },
            Rule::Capacity => {
                // Fail closed: a replica reporting neither `running` nor
                // `queued` gives no evidence it's idle, so treating both as
                // 0 would make it look falsely attractive. Exclude it
                // instead of guessing.
                if r.state.load.running.is_none() && r.state.load.queued.is_none() {
                    return Err(Exclusion(self));
                }
                if saturated(r) {
                    return Err(Exclusion(self));
                }
                Ok(())
            }
            Rule::Context => {
                // Fail open on either unknown: without an engine limit or a
                // requirement there is nothing to compare, and the pool's
                // context-length-400 self-heal still covers a wrong pick.
                match (input.context_tokens, r.state.limits.max_context_tokens) {
                    (Some(need), Some(max)) if need > max => Err(Exclusion(self)),
                    _ => Ok(()),
                }
            }
            Rule::Lane => Ok(()),
        }
    }
}

/// Whether `r` reports itself out of capacity: KV usage at or above
/// `KV_MAX`, or at least `2 * max_running` requests in flight. The part of
/// `Rule::Capacity` that is evidence of a full replica, as opposed to its
/// fail-closed exclusion of a replica that reports no counts at all.
pub fn saturated(r: &ReplicaView) -> bool {
    if r.state.load.kv_usage.is_some_and(|kv| kv >= KV_MAX) {
        return true;
    }
    r.state.limits.max_running.is_some_and(|m| {
        let running = r.state.load.running.unwrap_or(0);
        let queued = r.state.load.queued.unwrap_or(0);
        running.saturating_add(queued) >= m.saturating_mul(2)
    })
}

/// Returns the first stage-1 rule (in `RULES` order) that excludes `r`, or
/// `None` if `r` survives stage 1.
pub fn first_exclusion(r: &ReplicaView, input: &PlaceInput, now_ms: u64) -> Option<Exclusion> {
    RULES
        .into_iter()
        .find_map(|rule| rule.check(r, input, now_ms).err())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{input, view_ready, NOW};

    #[test]
    fn ready_fresh_passes() {
        let v = view_ready();
        assert_eq!(first_exclusion(&v, &input(), NOW), None);
    }

    #[test]
    fn first_failing_rule_is_reported() {
        let mut v = view_ready();
        v.state.lifecycle_state = Lifecycle::Warming;
        v.state.engine_sampled_at_ms = None;
        assert_eq!(
            first_exclusion(&v, &input(), NOW),
            Some(Exclusion(Rule::Lifecycle))
        );
    }

    #[test]
    fn far_future_sample_is_not_fresh() {
        let mut v = view_ready();
        v.state.engine_sampled_at_ms = Some(NOW + MAX_FUTURE_SKEW_MS + 1);
        assert_eq!(
            first_exclusion(&v, &input(), NOW),
            Some(Exclusion(Rule::Freshness))
        );
        v.state.engine_sampled_at_ms = Some(NOW + MAX_FUTURE_SKEW_MS);
        assert_eq!(first_exclusion(&v, &input(), NOW), None);
    }

    #[test]
    fn wedged_engine_is_stale() {
        let mut v = view_ready();
        v.state.engine_sampled_at_ms = Some(NOW - 10_000);
        assert_eq!(
            first_exclusion(&v, &input(), NOW),
            Some(Exclusion(Rule::Freshness))
        );
    }

    #[test]
    fn null_sample_time_is_stale() {
        let mut v = view_ready();
        v.state.engine_sampled_at_ms = None;
        assert_eq!(
            first_exclusion(&v, &input(), NOW),
            Some(Exclusion(Rule::Freshness))
        );
    }

    #[test]
    fn capacity_excludes_countless_replica() {
        let mut v = view_ready();
        v.state.load.running = None;
        v.state.load.queued = None;
        assert_eq!(
            first_exclusion(&v, &input(), NOW),
            Some(Exclusion(Rule::Capacity))
        );
    }

    #[test]
    fn kv_full() {
        let mut v = view_ready();
        v.state.load.kv_usage = Some(0.96);
        assert_eq!(
            first_exclusion(&v, &input(), NOW),
            Some(Exclusion(Rule::Capacity))
        );
    }

    #[test]
    fn over_capacity() {
        let mut v = view_ready();
        v.state.limits.max_running = Some(4);
        v.state.load.running = Some(6);
        v.state.load.queued = Some(2);
        assert_eq!(
            first_exclusion(&v, &input(), NOW),
            Some(Exclusion(Rule::Capacity))
        );
    }

    #[test]
    fn context_excludes_when_requirement_exceeds_engine_limit() {
        let mut v = view_ready();
        v.state.limits.max_context_tokens = Some(131_072);
        let mut inp = input();
        inp.context_tokens = Some(131_073);
        assert_eq!(
            first_exclusion(&v, &inp, NOW),
            Some(Exclusion(Rule::Context))
        );
    }

    #[test]
    fn unknown_context_limit_passes() {
        let mut v = view_ready();
        v.state.limits.max_context_tokens = None;
        let mut inp = input();
        inp.context_tokens = Some(1_000_000);
        assert_eq!(first_exclusion(&v, &inp, NOW), None);
    }

    #[test]
    fn unknown_requirement_passes() {
        let mut v = view_ready();
        v.state.limits.max_context_tokens = Some(8_192);
        let mut inp = input();
        inp.context_tokens = None;
        assert_eq!(first_exclusion(&v, &inp, NOW), None);
    }

    #[test]
    fn exact_limit_passes() {
        let mut v = view_ready();
        v.state.limits.max_context_tokens = Some(131_072);
        let mut inp = input();
        inp.context_tokens = Some(131_072);
        assert_eq!(first_exclusion(&v, &inp, NOW), None);
    }

    #[test]
    fn lane_is_not_a_stage_one_rule() {
        assert!(!RULES.contains(&Rule::Lane));
        assert_eq!(ALL_RULES[..4], RULES);
        assert_eq!(
            Rule::Lane.check(&view_ready(), &input(), NOW),
            Ok(()),
            "the lane is evaluated over all survivors, not per replica"
        );
    }

    #[test]
    fn every_rule_has_a_static_tag() {
        let tags: Vec<&str> = ALL_RULES.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            tags,
            ["lifecycle", "freshness", "capacity", "context", "lane"]
        );
    }
}
