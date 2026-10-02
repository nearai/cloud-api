//! Ordered eligibility rules (Chain of Responsibility), in two stages.
//!
//! Stage 1 is per replica: [`first_exclusion`] runs each [`Rule`] in
//! [`RULES`] order against a [`ReplicaView`] and returns the first one that
//! excludes it, or `None` if the replica survives. Stage 2 is `Rule::Lane`,
//! which needs a view of every live replica at once, so `Placer::place`
//! evaluates it after stage 1 (see `policy::lane_admits`). Exclusions from
//! both stages are tallied per rule in [`ALL_RULES`] order.

use crate::consts::{FRESH_MAX_MS, MAX_FUTURE_SKEW_MS};
use crate::decision::PlaceInput;
use crate::frame::Lifecycle;
use crate::snapshot::ReplicaView;
use crate::tuning::Tuning;

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
    pub fn check(
        self,
        r: &ReplicaView,
        input: &PlaceInput,
        now_ms: u64,
        tuning: &Tuning,
    ) -> Result<(), Exclusion> {
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
                // Fail closed: a replica missing any of `running`, `queued` or
                // `prefill_backlog_tokens` gives no evidence it's idle, so
                // treating it as 0 would make it look falsely attractive.
                // Exclude it instead of guessing.
                let load = &r.state.load;
                if load.running.is_none()
                    || load.queued.is_none()
                    || load.prefill_backlog_tokens.is_none()
                {
                    return Err(Exclusion(self));
                }
                if saturated(r, tuning.kv_max) {
                    return Err(Exclusion(self));
                }
                Ok(())
            }
            Rule::Context => {
                // Fail open on an unknown engine limit: with nothing to compare,
                // the pool's context-length-400 self-heal still covers a wrong
                // pick.
                match r.state.limits.max_context_tokens {
                    Some(max) if input.prompt_tokens > max => Err(Exclusion(self)),
                    _ => Ok(()),
                }
            }
            Rule::Lane => Ok(()),
        }
    }
}

/// Whether `r` reports itself out of capacity: KV usage at or above
/// `kv_max` (`Tuning::kv_max`). The part of `Rule::Capacity` that is evidence of a full replica,
/// as opposed to its fail-closed exclusion of a replica missing load counts.
/// Queue depth is not a signal: the engine caps running + queued itself.
pub fn saturated(r: &ReplicaView, kv_max: f64) -> bool {
    r.state.load.kv_usage.is_some_and(|kv| kv >= kv_max)
}

/// Returns the first stage-1 rule (in `RULES` order) that excludes `r`, or
/// `None` if `r` survives stage 1.
pub fn first_exclusion(
    r: &ReplicaView,
    input: &PlaceInput,
    now_ms: u64,
    tuning: &Tuning,
) -> Option<Exclusion> {
    RULES
        .into_iter()
        .find_map(|rule| rule.check(r, input, now_ms, tuning).err())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{input, view_ready, NOW};

    #[test]
    fn ready_fresh_passes() {
        let v = view_ready();
        assert_eq!(first_exclusion(&v, &input(), NOW, &Tuning::default()), None);
    }

    #[test]
    fn first_failing_rule_is_reported() {
        let mut v = view_ready();
        v.state.lifecycle_state = Lifecycle::Warming;
        v.state.engine_sampled_at_ms = None;
        assert_eq!(
            first_exclusion(&v, &input(), NOW, &Tuning::default()),
            Some(Exclusion(Rule::Lifecycle))
        );
    }

    #[test]
    fn far_future_sample_is_not_fresh() {
        let mut v = view_ready();
        v.state.engine_sampled_at_ms = Some(NOW + MAX_FUTURE_SKEW_MS + 1);
        assert_eq!(
            first_exclusion(&v, &input(), NOW, &Tuning::default()),
            Some(Exclusion(Rule::Freshness))
        );
        v.state.engine_sampled_at_ms = Some(NOW + MAX_FUTURE_SKEW_MS);
        assert_eq!(first_exclusion(&v, &input(), NOW, &Tuning::default()), None);
    }

    #[test]
    fn wedged_engine_is_stale() {
        let mut v = view_ready();
        v.state.engine_sampled_at_ms = Some(NOW - 10_000);
        assert_eq!(
            first_exclusion(&v, &input(), NOW, &Tuning::default()),
            Some(Exclusion(Rule::Freshness))
        );
    }

    #[test]
    fn null_sample_time_is_stale() {
        let mut v = view_ready();
        v.state.engine_sampled_at_ms = None;
        assert_eq!(
            first_exclusion(&v, &input(), NOW, &Tuning::default()),
            Some(Exclusion(Rule::Freshness))
        );
    }

    #[test]
    fn capacity_excludes_countless_replica() {
        let mut v = view_ready();
        v.state.load.running = None;
        v.state.load.queued = None;
        v.state.load.prefill_backlog_tokens = None;
        assert_eq!(
            first_exclusion(&v, &input(), NOW, &Tuning::default()),
            Some(Exclusion(Rule::Capacity))
        );
    }

    #[test]
    fn backlog_missing_with_queued_known_is_excluded() {
        let mut v = view_ready();
        v.state.load.queued = Some(3);
        v.state.load.prefill_backlog_tokens = None;
        assert_eq!(
            first_exclusion(&v, &input(), NOW, &Tuning::default()),
            Some(Exclusion(Rule::Capacity))
        );
    }

    #[test]
    fn queued_missing_is_excluded() {
        let mut v = view_ready();
        v.state.load.queued = None;
        assert_eq!(
            first_exclusion(&v, &input(), NOW, &Tuning::default()),
            Some(Exclusion(Rule::Capacity))
        );
    }

    #[test]
    fn running_missing_is_excluded() {
        let mut v = view_ready();
        v.state.load.running = None;
        assert_eq!(
            first_exclusion(&v, &input(), NOW, &Tuning::default()),
            Some(Exclusion(Rule::Capacity))
        );
    }

    #[test]
    fn kv_full() {
        let mut v = view_ready();
        v.state.load.kv_usage = Some(0.96);
        assert!(saturated(&v, Tuning::default().kv_max));
        assert_eq!(
            first_exclusion(&v, &input(), NOW, &Tuning::default()),
            Some(Exclusion(Rule::Capacity))
        );
    }

    #[test]
    fn kv_max_is_tunable() {
        let mut v = view_ready();
        v.state.load.kv_usage = Some(0.8);
        let tight = Tuning {
            kv_max: 0.7,
            ..Tuning::default()
        };
        assert!(!saturated(&v, 0.95));
        assert!(saturated(&v, tight.kv_max));
        assert_eq!(
            first_exclusion(&v, &input(), NOW, &tight),
            Some(Exclusion(Rule::Capacity))
        );
        assert_eq!(first_exclusion(&v, &input(), NOW, &Tuning::default()), None);
        // The bound is inclusive.
        v.state.load.kv_usage = Some(0.7);
        assert!(saturated(&v, 0.7));
    }

    #[test]
    fn over_capacity() {
        // Queue depth alone is not saturation: only KV is.
        let mut v = view_ready();
        v.state.limits.max_running = Some(4);
        v.state.load.running = Some(6);
        v.state.load.queued = Some(2);
        v.state.load.kv_usage = Some(0.5);
        assert!(!saturated(&v, Tuning::default().kv_max));
        assert_eq!(first_exclusion(&v, &input(), NOW, &Tuning::default()), None);
    }

    #[test]
    fn context_excludes_when_prompt_exceeds_engine_limit() {
        let mut v = view_ready();
        v.state.limits.max_context_tokens = Some(131_072);
        let mut inp = input();
        inp.prompt_tokens = 131_073;
        assert_eq!(
            first_exclusion(&v, &inp, NOW, &Tuning::default()),
            Some(Exclusion(Rule::Context))
        );
    }

    #[test]
    fn unknown_context_limit_passes() {
        let mut v = view_ready();
        v.state.limits.max_context_tokens = None;
        let mut inp = input();
        inp.prompt_tokens = 1_000_000;
        assert_eq!(first_exclusion(&v, &inp, NOW, &Tuning::default()), None);
    }

    #[test]
    fn exact_limit_passes() {
        let mut v = view_ready();
        v.state.limits.max_context_tokens = Some(131_072);
        let mut inp = input();
        inp.prompt_tokens = 131_072;
        assert_eq!(first_exclusion(&v, &inp, NOW, &Tuning::default()), None);
    }

    #[test]
    fn lane_is_not_a_stage_one_rule() {
        assert!(!RULES.contains(&Rule::Lane));
        assert_eq!(ALL_RULES[..4], RULES);
        assert_eq!(
            Rule::Lane.check(&view_ready(), &input(), NOW, &Tuning::default()),
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
