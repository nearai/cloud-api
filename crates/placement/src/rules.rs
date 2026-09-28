//! Ordered eligibility rules (Chain of Responsibility).
//!
//! [`first_exclusion`] runs each [`Rule`] in [`RULES`] order against a
//! [`ReplicaView`] and returns the first one that excludes it, or `None` if
//! the replica is eligible. Callers use this to filter a snapshot down to
//! the replicas a placement decision may pick from.

use crate::consts::{FRESH_MAX_MS, KV_MAX};
use crate::decision::PlaceInput;
use crate::frame::Lifecycle;
use crate::snapshot::ReplicaView;

/// A single eligibility check, in the order they are applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    Model,
    Lifecycle,
    Freshness,
    Capacity,
    Context,
}

/// A replica was excluded by `Rule`. Carries no request content — safe to
/// log (see `Rule::as_str`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Exclusion(pub Rule);

/// All rules, in the order `first_exclusion` applies them.
pub const RULES: [Rule; 5] = [
    Rule::Model,
    Rule::Lifecycle,
    Rule::Freshness,
    Rule::Capacity,
    Rule::Context,
];

impl Rule {
    /// A stable, content-free name for logging/metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::Model => "model",
            Rule::Lifecycle => "lifecycle",
            Rule::Freshness => "freshness",
            Rule::Capacity => "capacity",
            Rule::Context => "context",
        }
    }

    /// Checks `r` against this rule. `now_ms` is the caller's clock, used by
    /// `Freshness` to judge how old `engine_sampled_at_ms` is.
    pub fn check(self, r: &ReplicaView, input: &PlaceInput, now_ms: u64) -> Result<(), Exclusion> {
        match self {
            Rule::Model => {
                if r.report.model == input.model {
                    Ok(())
                } else {
                    Err(Exclusion(self))
                }
            }
            Rule::Lifecycle => {
                if r.report.lifecycle_state == Lifecycle::Ready {
                    Ok(())
                } else {
                    Err(Exclusion(self))
                }
            }
            Rule::Freshness => match r.report.engine_sampled_at_ms {
                // A future `t` counts as fresh: `saturating_sub` is 0.
                Some(t) if now_ms.saturating_sub(t) <= FRESH_MAX_MS => Ok(()),
                // `None`, or too old: a wedged engine keeps reporting an old
                // `t` forever, so a missing sample time is stale too (E9).
                _ => Err(Exclusion(self)),
            },
            Rule::Capacity => {
                // Fail closed: a replica reporting neither `running` nor
                // `queued` gives no evidence it's idle, so treating both as
                // 0 would make it look falsely attractive. Exclude it
                // instead of guessing.
                if r.report.load.running.is_none() && r.report.load.queued.is_none() {
                    return Err(Exclusion(self));
                }
                if let Some(kv) = r.report.load.kv_usage {
                    if kv >= KV_MAX {
                        return Err(Exclusion(self));
                    }
                }
                if let Some(m) = r.report.limits.max_running {
                    let running = r.report.load.running.unwrap_or(0);
                    let queued = r.report.load.queued.unwrap_or(0);
                    let in_flight = running.saturating_add(queued);
                    let cap = m.saturating_mul(2);
                    if in_flight >= cap {
                        return Err(Exclusion(self));
                    }
                }
                Ok(())
            }
            Rule::Context => {
                if input.prompt_tokens_est > 100_000
                    && !input.long_context_hosts.iter().any(|h| h == &r.host_id)
                {
                    return Err(Exclusion(self));
                }
                Ok(())
            }
        }
    }
}

/// Returns the first rule (in `RULES` order) that excludes `r`, or `None` if
/// `r` is eligible under every rule.
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
    fn model_mismatch() {
        let v = view_ready();
        let mut inp = input();
        inp.model = "some-other-model".into();
        assert_eq!(first_exclusion(&v, &inp, NOW), Some(Exclusion(Rule::Model)));
    }

    #[test]
    fn first_failing_rule_is_reported() {
        let mut v = view_ready();
        v.report.lifecycle_state = Lifecycle::Warming;
        v.report.engine_sampled_at_ms = None;
        assert_eq!(
            first_exclusion(&v, &input(), NOW),
            Some(Exclusion(Rule::Lifecycle))
        );
    }

    #[test]
    fn wedged_engine_is_stale() {
        let mut v = view_ready();
        v.report.engine_sampled_at_ms = Some(NOW - 10_000);
        v.report.reported_at_ms = NOW;
        assert_eq!(
            first_exclusion(&v, &input(), NOW),
            Some(Exclusion(Rule::Freshness))
        );
    }

    #[test]
    fn null_sample_time_is_stale() {
        let mut v = view_ready();
        v.report.engine_sampled_at_ms = None;
        assert_eq!(
            first_exclusion(&v, &input(), NOW),
            Some(Exclusion(Rule::Freshness))
        );
    }

    #[test]
    fn capacity_excludes_countless_replica() {
        let mut v = view_ready();
        v.report.load.running = None;
        v.report.load.queued = None;
        assert_eq!(
            first_exclusion(&v, &input(), NOW),
            Some(Exclusion(Rule::Capacity))
        );
    }

    #[test]
    fn kv_full() {
        let mut v = view_ready();
        v.report.load.kv_usage = Some(0.96);
        assert_eq!(
            first_exclusion(&v, &input(), NOW),
            Some(Exclusion(Rule::Capacity))
        );
    }

    #[test]
    fn over_capacity() {
        let mut v = view_ready();
        v.report.limits.max_running = Some(4);
        v.report.load.running = Some(6);
        v.report.load.queued = Some(2);
        assert_eq!(
            first_exclusion(&v, &input(), NOW),
            Some(Exclusion(Rule::Capacity))
        );
    }

    #[test]
    fn long_prompt_needs_long_host() {
        let v = view_ready();
        let mut inp = input();
        inp.prompt_tokens_est = 200_000;
        assert_eq!(
            first_exclusion(&v, &inp, NOW),
            Some(Exclusion(Rule::Context))
        );

        inp.long_context_hosts = vec![v.host_id.clone()];
        assert_eq!(first_exclusion(&v, &inp, NOW), None);
    }
}
