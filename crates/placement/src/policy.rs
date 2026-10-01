//! Long-context routing policy: the request's class, the Fleet's tier, the
//! heavy lane, and the route policy label.
//!
//! Both the class and the tier come from the pool, which owns the size
//! estimate and the tier boundary: a request is heavy for the lane when its
//! prompt alone exceeds the smallest declared capacity
//! (`PlaceInput::prefill_heavy`), and a Fleet is `Long` when its declared
//! capacity is above that. Placement never re-derives either from token
//! counts.
//!
//! The heavy lane is the only filter the class adds. [`lane_admits`] is the
//! whole admission predicate (applied as `Rule::Lane`), and [`classify`] only
//! labels the outcome: every class selects with the same HRW, pins and
//! score, so a pinned heavy conversation keeps its replica whenever the lane
//! admits it.

use std::collections::HashSet;

use crate::consts::{
    HEAVY_BACKLOG_CAP, HEAVY_BASE_MAX_PROMPT, HEAVY_SHARE, LANE_LOAD_TOKENS, LONG_BACKLOG_CAP,
};
use crate::snapshot::{ReplicaView, SlotId};

/// The capacity tier of the Fleet a `Placer` serves. Fixed at construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tier {
    Base,
    Long,
}

impl Tier {
    /// A stable, content-free name for logs and metric tags.
    pub const fn as_str(self) -> &'static str {
        match self {
            Tier::Base => "base",
            Tier::Long => "long",
        }
    }
}

/// A request's lane class, from `PlaceInput::prefill_heavy` (prompt size
/// alone over the base tier's capacity).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Short,
    Heavy,
}

impl Class {
    pub const fn of(heavy: bool) -> Self {
        if heavy {
            Class::Heavy
        } else {
            Class::Short
        }
    }

    /// A stable, content-free name for logs and metric tags.
    pub const fn as_str(self) -> &'static str {
        match self {
            Class::Short => "short",
            Class::Heavy => "heavy",
        }
    }
}

/// A request's priority, bucketed for the record: `neg` (< 0, never crosses
/// to another tier), `normal` (0) and `high` (> 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriorityBand {
    Neg,
    Normal,
    High,
}

impl PriorityBand {
    pub const fn of(priority: i32) -> Self {
        if priority < 0 {
            PriorityBand::Neg
        } else if priority == 0 {
            PriorityBand::Normal
        } else {
            PriorityBand::High
        }
    }

    /// A stable, content-free name for logs and metric tags.
    pub const fn as_str(self) -> &'static str {
        match self {
            PriorityBand::Neg => "neg",
            PriorityBand::Normal => "normal",
            PriorityBand::High => "high",
        }
    }
}

/// The heavy lane as of one decision, built from raw load only (a replica's
/// effective prefill backlog plus its pending tokens), so a request whose
/// size was underestimated still makes its replica a member.
///
/// Membership and the cap count every *live* replica (past `Rule::Lifecycle`
/// and `Rule::Freshness`), including one stage 1 then drops for `Capacity` or
/// `Context`: a loaded replica that is KV-full is still prefilling its heavy
/// work, and dropping it from the count would free a lane slot it holds.
///
/// The cap is a snapshot-based admission limit, not a distributed reservation.
/// Concurrent routers can admit different members before shared pending load
/// becomes visible, temporarily exceeding the cap. Callers should monitor the
/// observed lane size alongside the cap.
#[derive(Clone, Debug, Default)]
pub struct LaneView {
    /// Live slots whose load is at least `LANE_LOAD_TOKENS`.
    pub members: HashSet<SlotId>,
    pub size: usize,
    /// `ceil(live replicas * HEAVY_SHARE)`.
    pub cap: usize,
    /// Whether some stage-1 survivor is not a member: the only replicas a
    /// short request can be sent to.
    pub any_clean: bool,
}

/// Build the lane from every live replica and its load (`live`), and judge
/// `any_clean` over the stage-1 survivors (`survivors`, a subset of `live`).
pub fn lane_view(live: &[(&ReplicaView, u64)], survivors: &[&SlotId]) -> LaneView {
    let members: HashSet<SlotId> = live
        .iter()
        .filter(|(_, load)| *load >= LANE_LOAD_TOKENS)
        .map(|(v, _)| v.slot.clone())
        .collect();
    LaneView {
        cap: (live.len() as f64 * HEAVY_SHARE).ceil() as usize,
        any_clean: survivors.iter().any(|s| !members.contains(*s)),
        size: members.len(),
        members,
    }
}

/// The single admission predicate, applied to each stage-1 survivor as
/// `Rule::Lane`. `load` is the replica's lane load
/// (`score::effective_backlog` plus pending tokens), and `idle` says that
/// load is *known* to be zero: `score::known_idle` and nothing pending.
///
/// - Heavy on the long tier: the replica's load plus the prompt must fit
///   under `LONG_BACKLOG_CAP`, unless the replica is idle.
/// - Heavy on the base tier: the prompt must be at most
///   `HEAVY_BASE_MAX_PROMPT`, the load plus the prompt must fit under
///   `HEAVY_BACKLOG_CAP`, and the replica must already be a member or the
///   lane must have room for one more. No idle waiver: the prompt limit is
///   below the cap, so an idle replica always fits.
///
/// The long tier's idle waiver keeps a single prompt larger than the cap
/// servable: the cap bounds queueing behind other work, and an idle replica
/// has none. It needs known load, so a replica that omits a load field never
/// gets it. The engine's own context limit is `Rule::Context`'s job, not the
/// lane's.
/// - Short: a member is avoided while any clean replica survives; with none
///   clean, every survivor passes, so the lane never empties a short request.
pub fn lane_admits(
    tier: Tier,
    class: Class,
    load: u64,
    idle: bool,
    prompt: u64,
    is_member: bool,
    lane: &LaneView,
) -> bool {
    match (tier, class) {
        (Tier::Long, Class::Heavy) => idle || load.saturating_add(prompt) <= LONG_BACKLOG_CAP,
        (Tier::Base, Class::Heavy) => {
            prompt <= HEAVY_BASE_MAX_PROMPT
                && load.saturating_add(prompt) <= HEAVY_BACKLOG_CAP
                && (is_member || lane.size < lane.cap)
        }
        (_, Class::Short) => !is_member || !lane.any_clean,
    }
}

/// Which branch of the long-context decision tree a request took. A label
/// for the record and metrics only: it never changes the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutePolicy {
    /// Short, and a clean (non-member) replica was eligible.
    ShortClean,
    /// Short, and every eligible replica was a lane member.
    ShortOverflow,
    /// Heavy, placed on the long tier.
    HeavyLong,
    /// Heavy, placed on a replica already in the base lane.
    HeavyLaneJoin,
    /// Heavy, placed on a base replica that joins the lane.
    HeavyLaneAdmit,
}

impl RoutePolicy {
    /// A stable, content-free name for logs and metric tags.
    pub const fn as_str(self) -> &'static str {
        match self {
            RoutePolicy::ShortClean => "short_clean",
            RoutePolicy::ShortOverflow => "short_overflow",
            RoutePolicy::HeavyLong => "heavy_long",
            RoutePolicy::HeavyLaneJoin => "heavy_lane_join",
            RoutePolicy::HeavyLaneAdmit => "heavy_lane_admit",
        }
    }
}

/// Pure label for a placed decision: no scoring, no candidates.
pub fn classify(tier: Tier, class: Class, lane: &LaneView, chosen: &SlotId) -> RoutePolicy {
    match (class, tier) {
        (Class::Heavy, Tier::Long) => RoutePolicy::HeavyLong,
        (Class::Heavy, Tier::Base) if lane.members.contains(chosen) => RoutePolicy::HeavyLaneJoin,
        (Class::Heavy, Tier::Base) => RoutePolicy::HeavyLaneAdmit,
        (Class::Short, _) if lane.any_clean => RoutePolicy::ShortClean,
        (Class::Short, _) => RoutePolicy::ShortOverflow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{slot, view};

    #[test]
    fn every_tier_and_class_has_a_static_tag() {
        assert_eq!(Tier::Base.as_str(), "base");
        assert_eq!(Tier::Long.as_str(), "long");
        assert_eq!(Class::Short.as_str(), "short");
        assert_eq!(Class::Heavy.as_str(), "heavy");
        assert_eq!(Class::of(true), Class::Heavy);
        assert_eq!(Class::of(false), Class::Short);
    }

    fn with_backlog(replica: u32, backlog: u64) -> ReplicaView {
        let mut v = view("gpu01", replica);
        v.state.load.prefill_backlog_tokens = Some(backlog);
        v
    }

    fn lane_of(loads: &[u64]) -> LaneView {
        let views: Vec<ReplicaView> = (0..loads.len() as u32).map(|i| view("gpu01", i)).collect();
        let pairs: Vec<(&ReplicaView, u64)> = views.iter().zip(loads.iter().copied()).collect();
        let slots: Vec<&SlotId> = views.iter().map(|v| &v.slot).collect();
        lane_view(&pairs, &slots)
    }

    #[test]
    fn lane_view_counts_members_and_caps_at_a_quarter() {
        let lane = lane_of(&[LANE_LOAD_TOKENS, LANE_LOAD_TOKENS - 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(lane.size, 1);
        assert_eq!(lane.cap, 2, "ceil(8 * 0.25)");
        assert!(lane.members.contains(&slot("gpu01", 0)));
        assert!(!lane.members.contains(&slot("gpu01", 1)));
        assert!(lane.any_clean);

        // ceil rounds a single replica's share up to one lane slot.
        assert_eq!(lane_of(&[0]).cap, 1);
        assert_eq!(lane_of(&[0; 5]).cap, 2);
        assert_eq!(lane_of(&[]).cap, 0);

        let all_members = lane_of(&[LANE_LOAD_TOKENS; 3]);
        assert!(!all_members.any_clean);
        assert_eq!(all_members.size, 3);

        // A live member outside the survivors still counts; a clean live
        // replica outside them does not make the lane clean.
        let views = [view("gpu01", 0), view("gpu01", 1), view("gpu01", 2)];
        let live = [
            (&views[0], LANE_LOAD_TOKENS),
            (&views[1], 0),
            (&views[2], LANE_LOAD_TOKENS),
        ];
        let lane = lane_view(&live, &[&views[2].slot]);
        assert_eq!((lane.size, lane.cap), (2, 1));
        assert!(!lane.any_clean);
    }

    #[test]
    fn lane_view_reads_raw_backlog() {
        // The load passed in is what the caller computed (backlog + pending);
        // a view's own backlog alone doesn't make it a member here.
        let v = with_backlog(0, 70_000);
        let lane = lane_view(&[(&v, 0)], &[&v.slot]);
        assert_eq!(lane.size, 0);
        let lane = lane_view(&[(&v, 70_000)], &[&v.slot]);
        assert_eq!(lane.size, 1);
    }

    #[test]
    fn lane_member_counts_toward_cap() {
        // 4 replicas: cap 1. One member already fills the lane, so a heavy
        // request can join it but can't open a second member.
        let lane = lane_of(&[70_000, 0, 0, 0]);
        assert_eq!((lane.size, lane.cap), (1, 1));
        let prompt = 120_000;
        assert!(lane_admits(
            Tier::Base,
            Class::Heavy,
            70_000,
            false,
            prompt,
            true,
            &lane
        ));
        assert!(!lane_admits(
            Tier::Base,
            Class::Heavy,
            0,
            true,
            prompt,
            false,
            &lane
        ));

        // With room in the lane, a clean replica is admitted as a new member.
        let lane = lane_of(&[0, 0, 0, 0]);
        assert!(lane_admits(
            Tier::Base,
            Class::Heavy,
            0,
            true,
            prompt,
            false,
            &lane
        ));
    }

    #[test]
    fn heavy_admission_is_capped_by_tier() {
        let lane = lane_of(&[0, 0, 0, 0]);
        let at = |tier, load| lane_admits(tier, Class::Heavy, load, false, 100_000, false, &lane);
        assert!(at(Tier::Base, HEAVY_BACKLOG_CAP - 100_000));
        assert!(!at(Tier::Base, HEAVY_BACKLOG_CAP - 99_999));
        assert!(at(Tier::Long, LONG_BACKLOG_CAP - 100_000));
        assert!(!at(Tier::Long, LONG_BACKLOG_CAP - 99_999));
        // The long tier has no lane cap, only the backlog cap.
        let full = lane_of(&[LANE_LOAD_TOKENS, 0, 0, 0]);
        assert!(lane_admits(
            Tier::Long,
            Class::Heavy,
            0,
            true,
            100_000,
            false,
            &full
        ));
        // Saturates instead of wrapping.
        assert!(!lane_admits(
            Tier::Long,
            Class::Heavy,
            u64::MAX,
            false,
            1,
            true,
            &full
        ));
    }

    #[test]
    fn idle_long_replica_admits_prompt_larger_than_backlog_cap() {
        let lane = lane_of(&[0, 0]);
        let prompt = LONG_BACKLOG_CAP + 100_000; // e.g. 700K on a 1M engine
        assert!(lane_admits(
            Tier::Long,
            Class::Heavy,
            0,
            true,
            prompt,
            false,
            &lane
        ));
    }

    #[test]
    fn zero_load_without_known_idle_gets_no_waiver() {
        // A zero lane load read from missing fields is not idleness.
        let lane = lane_of(&[0, 0]);
        let prompt = LONG_BACKLOG_CAP + 1;
        for tier in [Tier::Base, Tier::Long] {
            assert!(!lane_admits(
                tier,
                Class::Heavy,
                0,
                false,
                prompt,
                true,
                &lane
            ));
        }
    }

    #[test]
    fn base_never_admits_prompt_over_heavy_base_max() {
        // An idle replica in a lane with room admits up to the limit, and
        // nothing over it, member or not.
        let room = lane_of(&[0, 0, 0, 0]);
        let at = |prompt, is_member| {
            lane_admits(Tier::Base, Class::Heavy, 0, true, prompt, is_member, &room)
        };
        assert!(at(HEAVY_BASE_MAX_PROMPT, false));
        assert!(!at(HEAVY_BASE_MAX_PROMPT + 1, false));
        assert!(!at(HEAVY_BASE_MAX_PROMPT + 1, true));
        // A full lane still keeps an idle non-member out.
        let full = lane_of(&[LANE_LOAD_TOKENS, 0, 0, 0]);
        assert!(!lane_admits(
            Tier::Base,
            Class::Heavy,
            0,
            true,
            HEAVY_BASE_MAX_PROMPT,
            false,
            &full
        ));
    }

    #[test]
    fn busy_replica_still_rejects_over_cap() {
        let lane = lane_of(&[0, 0, 0, 0]);
        assert!(!lane_admits(
            Tier::Long,
            Class::Heavy,
            1,
            false,
            LONG_BACKLOG_CAP,
            false,
            &lane
        ));
        // Base: a prompt within HEAVY_BASE_MAX_PROMPT, rejected on the cap.
        let prompt = HEAVY_BASE_MAX_PROMPT;
        assert!(!lane_admits(
            Tier::Base,
            Class::Heavy,
            HEAVY_BACKLOG_CAP - prompt + 1,
            false,
            prompt,
            false,
            &lane
        ));
        let member = lane_of(&[LANE_LOAD_TOKENS, 0, 0, 0]);
        let load = HEAVY_BACKLOG_CAP - 100_000 + 1;
        assert!(!lane_admits(
            Tier::Base,
            Class::Heavy,
            load,
            false,
            100_000,
            true,
            &member
        ));
    }

    #[test]
    fn short_avoids_members_only_while_a_clean_replica_exists() {
        let lane = lane_of(&[LANE_LOAD_TOKENS, 0]);
        for tier in [Tier::Base, Tier::Long] {
            assert!(!lane_admits(
                tier,
                Class::Short,
                LANE_LOAD_TOKENS,
                false,
                10,
                true,
                &lane
            ));
            assert!(lane_admits(tier, Class::Short, 0, true, 10, false, &lane));
        }
        let no_clean = lane_of(&[LANE_LOAD_TOKENS, LANE_LOAD_TOKENS]);
        assert!(lane_admits(
            Tier::Base,
            Class::Short,
            u64::MAX,
            false,
            10,
            true,
            &no_clean
        ));
    }

    #[test]
    fn classify_is_exhaustive_and_tagged() {
        let member = slot("gpu01", 0);
        let clean = slot("gpu01", 1);
        let lane = lane_of(&[LANE_LOAD_TOKENS, 0]);
        let no_clean = lane_of(&[LANE_LOAD_TOKENS, LANE_LOAD_TOKENS]);
        use RoutePolicy::*;
        let table = [
            (Tier::Long, Class::Heavy, &lane, &member, HeavyLong),
            (Tier::Long, Class::Heavy, &lane, &clean, HeavyLong),
            (Tier::Base, Class::Heavy, &lane, &member, HeavyLaneJoin),
            (Tier::Base, Class::Heavy, &lane, &clean, HeavyLaneAdmit),
            (Tier::Base, Class::Short, &lane, &clean, ShortClean),
            (Tier::Long, Class::Short, &lane, &clean, ShortClean),
            (Tier::Base, Class::Short, &no_clean, &member, ShortOverflow),
            (Tier::Long, Class::Short, &no_clean, &member, ShortOverflow),
        ];
        for (tier, class, lane, chosen, want) in table {
            assert_eq!(
                classify(tier, class, lane, chosen),
                want,
                "{tier:?} {class:?} {chosen:?}"
            );
        }

        let tags: Vec<&str> = [
            ShortClean,
            ShortOverflow,
            HeavyLong,
            HeavyLaneJoin,
            HeavyLaneAdmit,
        ]
        .iter()
        .map(|p| p.as_str())
        .collect();
        assert_eq!(
            tags,
            [
                "short_clean",
                "short_overflow",
                "heavy_long",
                "heavy_lane_join",
                "heavy_lane_admit"
            ]
        );
    }
}
