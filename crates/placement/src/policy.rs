//! Long-context routing policy: the request's class, the Fleet's tier, the
//! heavy lane, and the route policy label.
//!
//! Both the class and the tier come from the pool, which owns the size
//! estimate and the tier boundary: a request is heavy when its context
//! requirement exceeds the smallest declared capacity, and a Fleet is `Long`
//! when its declared capacity is above that. Placement never re-derives
//! either from token counts.
//!
//! The heavy lane is the only filter the class adds. [`lane_admits`] is the
//! whole admission predicate (applied as `Rule::Lane`), and [`classify`] only
//! labels the outcome: every class selects with the same HRW, pins and
//! score, so a pinned heavy conversation keeps its replica whenever the lane
//! admits it.

use std::collections::HashSet;

use crate::consts::{HEAVY_BACKLOG_CAP, HEAVY_SHARE, LANE_LOAD_TOKENS, LONG_BACKLOG_CAP};
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

/// A request's size class, from the pool's `PlacementContext.heavy`.
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

/// The heavy lane as of one decision, built from raw load only (a replica's
/// prefill backlog plus its pending tokens), so a request whose size was
/// underestimated still makes its replica a member.
#[derive(Clone, Debug, Default)]
pub struct LaneView {
    /// Slots whose load is at least `LANE_LOAD_TOKENS`.
    pub members: HashSet<SlotId>,
    pub size: usize,
    /// `ceil(stage-1 survivors * HEAVY_SHARE)`.
    pub cap: usize,
    /// Whether some stage-1 survivor is not a member.
    pub any_clean: bool,
}

/// Build the lane from the stage-1 survivors (Lifecycle..Context) and each
/// one's load (`prefill_backlog_tokens.unwrap_or(0) + pending tokens`).
pub fn lane_view(pre_lane: &[(&ReplicaView, u64)]) -> LaneView {
    let members: HashSet<SlotId> = pre_lane
        .iter()
        .filter(|(_, load)| *load >= LANE_LOAD_TOKENS)
        .map(|(v, _)| v.slot.clone())
        .collect();
    let size = members.len();
    LaneView {
        cap: (pre_lane.len() as f64 * HEAVY_SHARE).ceil() as usize,
        any_clean: size < pre_lane.len(),
        members,
        size,
    }
}

/// The single admission predicate, applied to each stage-1 survivor as
/// `Rule::Lane`.
///
/// - Heavy on the long tier: the replica's load plus the prompt must fit
///   under `LONG_BACKLOG_CAP`.
/// - Heavy on the base tier: the same under `HEAVY_BACKLOG_CAP`, and the
///   replica must already be a member or the lane must have room for one
///   more.
/// - Short: a member is avoided while any clean replica survives; with none
///   clean, every survivor passes, so short requests are never refused.
pub fn lane_admits(
    tier: Tier,
    class: Class,
    load: u64,
    prompt: u64,
    is_member: bool,
    lane: &LaneView,
) -> bool {
    match (tier, class) {
        (Tier::Long, Class::Heavy) => load.saturating_add(prompt) <= LONG_BACKLOG_CAP,
        (Tier::Base, Class::Heavy) => {
            load.saturating_add(prompt) <= HEAVY_BACKLOG_CAP && (is_member || lane.size < lane.cap)
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
    /// Heavy, and the lane excluded every replica that survived stage 1.
    Refuse,
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
            RoutePolicy::Refuse => "refuse",
        }
    }
}

/// Pure label: no scoring, no candidates. `chosen` is `None` when nothing
/// survived the rules.
pub fn classify(tier: Tier, class: Class, lane: &LaneView, chosen: Option<&SlotId>) -> RoutePolicy {
    match (class, tier, chosen) {
        (Class::Heavy, _, None) => RoutePolicy::Refuse,
        (Class::Heavy, Tier::Long, Some(_)) => RoutePolicy::HeavyLong,
        (Class::Heavy, Tier::Base, Some(s)) if lane.members.contains(s) => {
            RoutePolicy::HeavyLaneJoin
        }
        (Class::Heavy, Tier::Base, Some(_)) => RoutePolicy::HeavyLaneAdmit,
        (Class::Short, _, _) if lane.any_clean => RoutePolicy::ShortClean,
        (Class::Short, _, _) => RoutePolicy::ShortOverflow,
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
        lane_view(&pairs)
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
    }

    #[test]
    fn lane_view_reads_raw_backlog() {
        // The load passed in is what the caller computed (backlog + pending);
        // a view's own backlog alone doesn't make it a member here.
        let v = with_backlog(0, 70_000);
        let lane = lane_view(&[(&v, 0)]);
        assert_eq!(lane.size, 0);
        let lane = lane_view(&[(&v, 70_000)]);
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
            prompt,
            true,
            &lane
        ));
        assert!(!lane_admits(
            Tier::Base,
            Class::Heavy,
            0,
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
            prompt,
            false,
            &lane
        ));
    }

    #[test]
    fn heavy_admission_is_capped_by_tier() {
        let lane = lane_of(&[0, 0, 0, 0]);
        let at = |tier, load| lane_admits(tier, Class::Heavy, load, 100_000, false, &lane);
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
            100_000,
            false,
            &full
        ));
        // Saturates instead of wrapping.
        assert!(!lane_admits(
            Tier::Long,
            Class::Heavy,
            u64::MAX,
            1,
            true,
            &full
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
                10,
                true,
                &lane
            ));
            assert!(lane_admits(tier, Class::Short, 0, 10, false, &lane));
        }
        let no_clean = lane_of(&[LANE_LOAD_TOKENS, LANE_LOAD_TOKENS]);
        assert!(lane_admits(
            Tier::Base,
            Class::Short,
            u64::MAX,
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
            (Tier::Base, Class::Heavy, &lane, None, Refuse),
            (Tier::Long, Class::Heavy, &lane, None, Refuse),
            (Tier::Long, Class::Heavy, &lane, Some(&member), HeavyLong),
            (Tier::Long, Class::Heavy, &lane, Some(&clean), HeavyLong),
            (
                Tier::Base,
                Class::Heavy,
                &lane,
                Some(&member),
                HeavyLaneJoin,
            ),
            (
                Tier::Base,
                Class::Heavy,
                &lane,
                Some(&clean),
                HeavyLaneAdmit,
            ),
            (Tier::Base, Class::Short, &lane, Some(&clean), ShortClean),
            (Tier::Long, Class::Short, &lane, Some(&clean), ShortClean),
            (
                Tier::Base,
                Class::Short,
                &no_clean,
                Some(&member),
                ShortOverflow,
            ),
            (
                Tier::Long,
                Class::Short,
                &no_clean,
                Some(&member),
                ShortOverflow,
            ),
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
            Refuse,
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
                "heavy_lane_admit",
                "refuse"
            ]
        );
    }
}
