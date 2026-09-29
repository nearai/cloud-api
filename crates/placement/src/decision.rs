//! The `Placer` facade: ties `rules.rs`, `score.rs`, and `affinity.rs`
//! together into a single placement decision, plus the `DecisionRecord` the
//! caller logs (IDs and numbers only — see the crate's privacy rules).
//!
//! A decision picks one replica slot (`host#index`), scoring every eligible
//! slot on its own state and its own pending load. The rules run in two
//! stages: the per-replica rules, then the heavy lane over their survivors.

use std::collections::HashMap;
use std::time::Instant;

use rand::Rng;

use crate::affinity::{pin_id, select, AffinityKey, PinId, Selection};
use crate::consts::{COVERED_MODELS, FRESH_MAX_MS, PIN_TTL_MS};
use crate::policy::{classify, lane_admits, lane_view, Class, LaneView, PriorityBand, Tier};
use crate::rules::{first_exclusion, Rule, ALL_RULES};
use crate::score::{fleet_median_tps, pending_for, replica_score, Pending};
use crate::snapshot::{ReplicaView, SlotId, Snapshot};

/// Per-request inputs the eligibility rules (`rules.rs`) check a
/// [`crate::snapshot::ReplicaView`] against, plus the rest of what
/// `Placer::place` needs to score and pick a slot.
///
/// The token counts and `heavy` come from the pool's `PlacementContext`;
/// placement never estimates a request's size itself.
///
/// `affinity` holds an [`AffinityKey`], which has no `Debug`/`Display`, so
/// `PlaceInput` implements `Debug` manually and redacts it (see the manual
/// `impl` below) rather than deriving it.
#[derive(Clone)]
pub struct PlaceInput {
    pub model: String,
    /// Input tokens only (0 if unknown): prefill cost and lane load.
    pub prompt_tokens: u64,
    /// Input plus output reserve, checked against each replica's engine
    /// `max_context_tokens` by `Rule::Context`. `None` if unknown.
    pub context_tokens: Option<u64>,
    /// The pool's class decision: the requirement exceeds the base tier.
    pub heavy: bool,
    /// `params.request_priority`.
    pub priority: i32,
    /// The caller-derived affinity key (e.g. from a conversation id), if
    /// this request carries one. Never logged — see `affinity.rs`.
    pub affinity: Option<AffinityKey>,
    pub affinity_source: AffinitySource,
    pub now_ms: u64,
}

impl std::fmt::Debug for PlaceInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaceInput")
            .field("model", &self.model)
            .field("prompt_tokens", &self.prompt_tokens)
            .field("context_tokens", &self.context_tokens)
            .field("heavy", &self.heavy)
            .field("priority", &self.priority)
            .field("affinity", &self.affinity.is_some())
            .field("affinity_source", &self.affinity_source)
            .field("now_ms", &self.now_ms)
            .finish()
    }
}

/// Where `PlaceInput::affinity` came from, for the (content-free)
/// `DecisionRecord::affinity` field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AffinitySource {
    Client,
    Prefix,
    #[default]
    None,
}

impl AffinitySource {
    /// A stable, content-free name for logs and metric tags.
    pub const fn as_str(self) -> &'static str {
        match self {
            AffinitySource::Client => "client",
            AffinitySource::Prefix => "prefix",
            AffinitySource::None => "none",
        }
    }
}

/// Why a request fell back to the legacy `Fleet::acquire_index` path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LegacyReason {
    /// The snapshot is disabled by the data-plane kill switch.
    Disabled,
    NotCovered,
    NoState,
    Stale,
    NoneEligible,
    /// Reserved for the caller: the chosen host isn't in its host map.
    HostUnmapped,
    /// Reserved for the caller: the chosen host's index is outside the
    /// E2EE-pinned model key's backend group.
    KeyGroup,
    /// Reserved for the caller: the host map does not cover every backend,
    /// or a mapped host has no replica view in the snapshot. Placing then
    /// would starve the hosts the placer cannot see.
    Incomplete,
}

impl LegacyReason {
    /// A stable, content-free name for logs and metric tags.
    pub const fn as_str(self) -> &'static str {
        match self {
            LegacyReason::Disabled => "disabled",
            LegacyReason::NotCovered => "not_covered",
            LegacyReason::NoState => "no_state",
            LegacyReason::Stale => "stale",
            LegacyReason::NoneEligible => "none_eligible",
            LegacyReason::HostUnmapped => "host_unmapped",
            LegacyReason::KeyGroup => "key_group",
            LegacyReason::Incomplete => "incomplete",
        }
    }
}

/// The outcome of a placement decision. Holds a `PinId` (no `Debug`), so
/// this type intentionally does not derive `Debug`.
///
/// `Legacy` means no usable state, so the caller falls back. `Refused`
/// means there is state and every replica that survived stage 1 was
/// excluded by the heavy lane (only possible for a heavy request).
pub enum Decision {
    Place {
        slot: SlotId,
        record: DecisionRecord,
        pin_write: Option<(PinId, SlotId)>,
    },
    Refused {
        record: DecisionRecord,
    },
    Legacy {
        reason: LegacyReason,
        record: DecisionRecord,
    },
}

impl Decision {
    /// The decision's record, whatever the outcome.
    pub fn record(&self) -> &DecisionRecord {
        match self {
            Decision::Place { record, .. }
            | Decision::Refused { record }
            | Decision::Legacy { record, .. } => record,
        }
    }

    fn record_mut(&mut self) -> &mut DecisionRecord {
        match self {
            Decision::Place { record, .. }
            | Decision::Refused { record }
            | Decision::Legacy { record, .. } => record,
        }
    }
}

/// A content-free record of a placement decision, safe to log as-is (IDs
/// and numbers only). Must never carry `AffinityKey` or `PinId` bytes/hex.
#[derive(Clone, Debug)]
pub struct DecisionRecord {
    /// `place`, `refused` or `legacy`.
    pub outcome: &'static str,
    /// The legacy reason, or `lane_full`/`long_full` for a refusal.
    pub reason: Option<&'static str>,
    pub tier: Tier,
    pub class: Class,
    /// `RoutePolicy::as_str`, for placed and refused decisions.
    pub strategy: Option<&'static str>,
    /// `PriorityBand::as_str`.
    pub priority_band: &'static str,
    /// Lane members and the lane cap, as seen by this decision (0 when it
    /// never got as far as the lane).
    pub lane_size: u16,
    pub lane_cap: u16,
    pub prompt_tokens: u64,
    pub context_tokens: Option<u64>,
    /// Time spent in `Placer::place`, in microseconds.
    pub place_us: u32,
    pub rank: Option<u8>,
    pub affinity: &'static str,
    pub selection: Option<&'static str>,
    /// The chosen slot's `hrw_label` (`host#replica`).
    pub slot: Option<String>,
    /// The chosen slot's replica index.
    pub replica: Option<u32>,
    /// The key's HRW home slot label, for keyed selections.
    pub home: Option<String>,
    /// The live follow pin's slot label, if one was found.
    pub pinned: Option<String>,
    /// Number of eligible replica slots the decision scored over.
    pub eligible: u16,
    /// Exclusions per rule, in `ALL_RULES` order.
    pub excluded: [(Rule, u16); 5],
    pub chosen_score: Option<f64>,
    pub home_score: Option<f64>,
    pub best_score: Option<f64>,
    pub snapshot_age_ms: u64,
    pub pending_req: u32,
    /// Pending prompt tokens on the chosen slot (0 when none chosen).
    pub pending_tok: u64,
    pub chosen_backlog_tokens: Option<u64>,
}

impl DecisionRecord {
    /// A record with the request's own fields filled in and nothing chosen.
    fn empty(input: &PlaceInput, snap: &Snapshot, tier: Tier, outcome: &'static str) -> Self {
        DecisionRecord {
            outcome,
            reason: None,
            tier,
            class: Class::of(input.heavy),
            strategy: None,
            priority_band: PriorityBand::of(input.priority).as_str(),
            lane_size: 0,
            lane_cap: 0,
            prompt_tokens: input.prompt_tokens,
            context_tokens: input.context_tokens,
            place_us: 0,
            rank: None,
            affinity: input.affinity_source.as_str(),
            selection: None,
            slot: None,
            replica: None,
            home: None,
            pinned: None,
            eligible: 0,
            excluded: ALL_RULES.map(|r| (r, 0)),
            chosen_score: None,
            home_score: None,
            best_score: None,
            snapshot_age_ms: input.now_ms.saturating_sub(snap.built_ms),
            pending_req: 0,
            pending_tok: 0,
            chosen_backlog_tokens: None,
        }
    }
}

/// Exclusion tally, one counter per rule in `ALL_RULES` order.
#[derive(Default)]
struct Tally([u16; 5]);

impl Tally {
    fn add(&mut self, rule: Rule) {
        if let Some(i) = ALL_RULES.iter().position(|r| *r == rule) {
            self.0[i] = self.0[i].saturating_add(1);
        }
    }

    fn record(&self) -> [(Rule, u16); 5] {
        std::array::from_fn(|i| (ALL_RULES[i], self.0[i]))
    }
}

/// A stage-1 survivor, the pending load it is scored with, and its lane
/// load (`prefill_backlog_tokens.unwrap_or(0) + pending.tok`).
struct Candidate<'a> {
    view: &'a ReplicaView,
    pending: Pending,
    load: u64,
}

/// The pure placement decision-maker for one Fleet. Holds only the
/// deployment's pin secret and the Fleet's tier — no I/O, no mutable state.
pub struct Placer {
    pin_secret: [u8; 32],
    tier: Tier,
}

impl Placer {
    pub fn new(pin_secret: [u8; 32], tier: Tier) -> Self {
        Self { pin_secret, tier }
    }

    /// The capacity tier this placer serves.
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Decide where `input` goes on this Fleet, given the latest snapshot and
    /// the part of this node's own ledger that `snap.routed` cannot include
    /// yet (`mine`, per slot: [`crate::score::unseen_by_read`] of the slot's
    /// ledger against `snap.routed_read_ms`; see
    /// [`crate::score::pending_for`]). `Legacy` when
    /// the snapshot is disabled, uncovered, empty, stale or has no stage-1
    /// survivor; `Refused` only when the heavy lane excluded every survivor;
    /// otherwise `Place`. The record's `place_us` times the whole call.
    pub fn place(
        &self,
        input: &PlaceInput,
        snap: &Snapshot,
        mine: &HashMap<SlotId, Pending>,
        rng: &mut impl Rng,
    ) -> Decision {
        let started = Instant::now();
        let mut decision = self.decide(input, snap, mine, rng);
        decision.record_mut().place_us =
            u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
        decision
    }

    fn decide(
        &self,
        input: &PlaceInput,
        snap: &Snapshot,
        mine: &HashMap<SlotId, Pending>,
        rng: &mut impl Rng,
    ) -> Decision {
        if snap.disabled {
            return self.legacy(input, snap, LegacyReason::Disabled, None);
        }

        if !COVERED_MODELS.contains(&input.model.as_str()) {
            return self.legacy(input, snap, LegacyReason::NotCovered, None);
        }

        if snap.replicas.is_empty() {
            return self.legacy(input, snap, LegacyReason::NoState, None);
        }

        if input.now_ms.saturating_sub(snap.built_ms) > FRESH_MAX_MS {
            return self.legacy(input, snap, LegacyReason::Stale, None);
        }

        // Stage 1: per-replica rules, tallying exclusions for the record.
        let mut tally = Tally::default();
        let mut candidates: Vec<Candidate<'_>> = Vec::with_capacity(snap.replicas.len());
        for view in &snap.replicas {
            match first_exclusion(view, input, input.now_ms) {
                None => {
                    let pending = pending_for(
                        snap.routed.get(&view.slot),
                        mine.get(&view.slot).copied().unwrap_or_default(),
                    );
                    let load = view
                        .state
                        .load
                        .prefill_backlog_tokens
                        .unwrap_or(0)
                        .saturating_add(pending.tok);
                    candidates.push(Candidate {
                        view,
                        pending,
                        load,
                    });
                }
                Some(exclusion) => tally.add(exclusion.0),
            }
        }

        // No stage-1 survivor (stale, not ready, over context, ...) is a
        // state problem, never a capacity one: fall back, don't refuse.
        if candidates.is_empty() {
            return self.legacy(
                input,
                snap,
                LegacyReason::NoneEligible,
                Some(tally.record()),
            );
        }

        // Stage 2: the heavy lane, over every stage-1 survivor at once.
        let class = Class::of(input.heavy);
        let lane = {
            let pre_lane: Vec<(&ReplicaView, u64)> =
                candidates.iter().map(|c| (c.view, c.load)).collect();
            lane_view(&pre_lane)
        };
        candidates.retain(|c| {
            let is_member = lane.members.contains(&c.view.slot);
            let admitted = lane_admits(
                self.tier,
                class,
                c.load,
                input.prompt_tokens,
                is_member,
                &lane,
            );
            if !admitted {
                tally.add(Rule::Lane);
            }
            admitted
        });

        if candidates.is_empty() {
            // Only capacity excluded the survivors. `lane_admits` always
            // admits a short request when anything survived stage 1, so this
            // is heavy in practice; a short request would still fall back.
            return match class {
                Class::Heavy => self.refused(input, snap, &lane, class, tally.record()),
                Class::Short => self.legacy(
                    input,
                    snap,
                    LegacyReason::NoneEligible,
                    Some(tally.record()),
                ),
            };
        }

        // The snapshot's replica order is the reader's; `select`'s keyless
        // BestOfTwo samples indices against `scores`' order, so two `Placer`s
        // (or the same one on a re-run) must see the same order to draw the
        // same conclusion from the same rng seed — sort by slot.
        candidates.sort_by(|a, b| a.view.slot.cmp(&b.view.slot));
        let views: Vec<&ReplicaView> = candidates.iter().map(|c| c.view).collect();
        let median = fleet_median_tps(&views);
        let scores: Vec<(SlotId, f64)> = candidates
            .iter()
            .map(|c| {
                (
                    c.view.slot.clone(),
                    replica_score(c.view, c.pending, median),
                )
            })
            .collect();
        let best_score = scores.iter().map(|(_, s)| *s).fold(f64::INFINITY, f64::min);

        // Pin lookup: only when this request carries an affinity key.
        let pin_id_opt = input
            .affinity
            .as_ref()
            .map(|k| pin_id(self.tier, k, &self.pin_secret));
        let pin_lookup: Option<(SlotId, u64)> = pin_id_opt.as_ref().and_then(|pid| {
            snap.pins
                .get(pid, input.now_ms)
                .map(|(slot, at_ms)| (slot.clone(), at_ms))
        });

        let selected = match select(
            input.affinity.as_ref(),
            pin_lookup.as_ref().map(|(s, _)| s),
            // Heavy: a pin that survived both rule stages holds regardless of
            // score; the lane's backlog caps already bound its load.
            input.heavy,
            &scores,
            rng,
        ) {
            Some(s) => s,
            None => {
                return self.legacy(
                    input,
                    snap,
                    LegacyReason::NoneEligible,
                    Some(tally.record()),
                )
            }
        };

        let rank: Option<u8> = match selected.selection {
            Selection::Home => Some(1),
            Selection::Spill { rank } => Some(rank),
            Selection::Pinned | Selection::BestOfTwo => None,
        };

        // A keyed heavy placement always (re)writes its pin: lane membership
        // follows the prefill backlog, so once it drains, the survivor set
        // and the HRW walk over it can change, and only a pin keeps the next
        // turn on its warm replica. Short requests pin only on a move or a
        // half-TTL refresh.
        let mut pin_write: Option<(PinId, SlotId)> = None;
        if let Some(pid) = pin_id_opt {
            let should_write = input.heavy
                || match selected.selection {
                    Selection::Pinned => pin_lookup
                        .as_ref()
                        .map(|(_, at_ms)| input.now_ms.saturating_sub(*at_ms) > PIN_TTL_MS / 2)
                        .unwrap_or(false),
                    _ => selected.write_pin,
                };
            if should_write {
                pin_write = Some((pid, selected.slot.clone()));
            }
        }

        let score_of = |slot: &SlotId| scores.iter().find(|(s, _)| s == slot).map(|(_, v)| *v);
        let chosen = candidates.iter().find(|c| c.view.slot == selected.slot);

        let record = DecisionRecord {
            outcome: "place",
            reason: None,
            tier: self.tier,
            class,
            strategy: Some(classify(self.tier, class, &lane, Some(&selected.slot)).as_str()),
            priority_band: PriorityBand::of(input.priority).as_str(),
            lane_size: u16::try_from(lane.size).unwrap_or(u16::MAX),
            lane_cap: u16::try_from(lane.cap).unwrap_or(u16::MAX),
            prompt_tokens: input.prompt_tokens,
            context_tokens: input.context_tokens,
            place_us: 0,
            rank,
            affinity: input.affinity_source.as_str(),
            selection: Some(selected.selection.as_str()),
            slot: Some(selected.slot.hrw_label()),
            replica: Some(selected.slot.replica),
            home: selected.home.as_ref().map(SlotId::hrw_label),
            pinned: pin_lookup.map(|(s, _)| s.hrw_label()),
            eligible: u16::try_from(candidates.len()).unwrap_or(u16::MAX),
            excluded: tally.record(),
            chosen_score: score_of(&selected.slot),
            home_score: selected.home.as_ref().and_then(score_of),
            best_score: Some(best_score),
            snapshot_age_ms: input.now_ms.saturating_sub(snap.built_ms),
            pending_req: chosen.map(|c| c.pending.req).unwrap_or(0),
            pending_tok: chosen.map(|c| c.pending.tok).unwrap_or(0),
            chosen_backlog_tokens: chosen.and_then(|c| c.view.state.load.prefill_backlog_tokens),
        };

        Decision::Place {
            slot: selected.slot,
            record,
            pin_write,
        }
    }

    /// Builds a `Decision::Legacy` for `reason`, optionally overriding the
    /// per-`Rule` exclusion tally (used once eligibility has already been
    /// computed, e.g. for `NoneEligible`).
    fn legacy(
        &self,
        input: &PlaceInput,
        snap: &Snapshot,
        reason: LegacyReason,
        excluded: Option<[(Rule, u16); 5]>,
    ) -> Decision {
        let mut record = DecisionRecord::empty(input, snap, self.tier, "legacy");
        record.reason = Some(reason.as_str());
        if let Some(excluded) = excluded {
            record.excluded = excluded;
        }
        Decision::Legacy { reason, record }
    }

    /// Builds a `Decision::Refused`: `long_full` on the long tier (every
    /// survivor over `LONG_BACKLOG_CAP`), `lane_full` on base.
    fn refused(
        &self,
        input: &PlaceInput,
        snap: &Snapshot,
        lane: &LaneView,
        class: Class,
        excluded: [(Rule, u16); 5],
    ) -> Decision {
        let mut record = DecisionRecord::empty(input, snap, self.tier, "refused");
        record.reason = Some(match self.tier {
            Tier::Long => "long_full",
            Tier::Base => "lane_full",
        });
        record.strategy = Some(classify(self.tier, class, lane, None).as_str());
        record.lane_size = u16::try_from(lane.size).unwrap_or(u16::MAX);
        record.lane_cap = u16::try_from(lane.cap).unwrap_or(u16::MAX);
        record.excluded = excluded;
        Decision::Refused { record }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affinity::{hrw_rank, AffinityKey};
    use crate::consts::{COVERED_MODELS, FRESH_MAX_MS, PIN_TTL_MS};
    use crate::consts::{HEAVY_BACKLOG_CAP, LANE_LOAD_TOKENS, LONG_BACKLOG_CAP};
    use crate::policy::{Class, PriorityBand, Tier};
    use crate::snapshot::RoutedCounts;
    use crate::testkit::{input as base_input, slot, view as ready_view, NOW};
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn snap_with(views: Vec<ReplicaView>) -> Snapshot {
        Snapshot {
            built_ms: NOW,
            replicas: views,
            routed: HashMap::new(),
            routed_read_ms: 0,
            pins: Default::default(),
            disabled: false,
            norefuse: false,
        }
    }

    fn placer() -> Placer {
        Placer::new([1u8; 32], Tier::Base)
    }

    fn keyed(key: AffinityKey) -> PlaceInput {
        let mut input = base_input();
        input.affinity = Some(key);
        input.affinity_source = AffinitySource::Client;
        input
    }

    fn placed(d: Decision) -> (SlotId, DecisionRecord, Option<(PinId, SlotId)>) {
        match d {
            Decision::Place {
                slot,
                record,
                pin_write,
            } => (slot, record, pin_write),
            Decision::Legacy { reason, .. } => panic!("expected Place, got Legacy({reason:?})"),
            Decision::Refused { .. } => panic!("expected Place, got Refused"),
        }
    }

    fn legacy_reason(d: Decision) -> (LegacyReason, DecisionRecord) {
        match d {
            Decision::Legacy { reason, record } => (reason, record),
            Decision::Place { .. } => panic!("expected Legacy, got Place"),
            Decision::Refused { .. } => panic!("expected Legacy, got Refused"),
        }
    }

    /// Searches for an `AffinityKey` whose HRW rank over `slots` puts `home`
    /// first, so a test can force a specific `Selection::Home`/`Spill`
    /// outcome instead of depending on whichever slot an arbitrary key
    /// happens to rank first.
    fn find_key_with_home(slots: &[SlotId], home: &SlotId) -> AffinityKey {
        let refs: Vec<&SlotId> = slots.iter().collect();
        for seed in 0u128.. {
            let key = AffinityKey::from_bytes(seed.to_be_bytes());
            if hrw_rank(&key, &refs).first() == Some(home) {
                return key;
            }
        }
        unreachable!("no key found within u128 search space")
    }

    #[test]
    fn caller_legacy_reasons_have_stable_names() {
        assert_eq!(LegacyReason::HostUnmapped.as_str(), "host_unmapped");
        assert_eq!(LegacyReason::KeyGroup.as_str(), "key_group");
        assert_eq!(LegacyReason::Incomplete.as_str(), "incomplete");
        assert_eq!(LegacyReason::Disabled.as_str(), "disabled");
    }

    #[test]
    fn disabled_snapshot_is_legacy_disabled() {
        // Disabled wins over every other check, including a usable snapshot.
        let mut snap = snap_with(vec![ready_view("gpu01", 0)]);
        snap.disabled = true;
        let mut rng = StdRng::seed_from_u64(1);
        let (reason, record) =
            legacy_reason(placer().place(&base_input(), &snap, &HashMap::new(), &mut rng));
        assert_eq!(reason, LegacyReason::Disabled);
        assert_eq!(record.reason, Some("disabled"));
        assert_eq!(record.outcome, "legacy");

        let mut uncovered = base_input();
        uncovered.model = "some-other-model".into();
        let (reason, _) =
            legacy_reason(placer().place(&uncovered, &snap, &HashMap::new(), &mut rng));
        assert_eq!(reason, LegacyReason::Disabled);
    }

    #[test]
    fn not_covered_is_legacy() {
        let mut input = base_input();
        input.model = "some-other-model".into();
        let snap = snap_with(vec![ready_view("gpu01", 0)]);
        let mut rng = StdRng::seed_from_u64(1);
        let (reason, record) =
            legacy_reason(placer().place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(reason, LegacyReason::NotCovered);
        assert_eq!(record.reason, Some("not_covered"));
        assert_eq!(record.outcome, "legacy");
    }

    #[test]
    fn no_state_is_legacy() {
        let snap = snap_with(vec![]);
        let mut rng = StdRng::seed_from_u64(1);
        let (reason, _) =
            legacy_reason(placer().place(&base_input(), &snap, &HashMap::new(), &mut rng));
        assert_eq!(reason, LegacyReason::NoState);
    }

    #[test]
    fn stale_snapshot_is_legacy() {
        let mut snap = snap_with(vec![ready_view("gpu01", 0)]);
        snap.built_ms = NOW - FRESH_MAX_MS - 1;
        let mut rng = StdRng::seed_from_u64(1);
        let (reason, record) =
            legacy_reason(placer().place(&base_input(), &snap, &HashMap::new(), &mut rng));
        assert_eq!(reason, LegacyReason::Stale);
        assert_eq!(record.reason, Some("stale"));
    }

    #[test]
    fn none_eligible_is_legacy() {
        let mut v = ready_view("gpu01", 0);
        v.state.lifecycle_state = crate::frame::Lifecycle::Warming;
        let snap = snap_with(vec![v]);
        let mut rng = StdRng::seed_from_u64(1);
        let (reason, record) =
            legacy_reason(placer().place(&base_input(), &snap, &HashMap::new(), &mut rng));
        assert_eq!(reason, LegacyReason::NoneEligible);
        assert_eq!(record.reason, Some("none_eligible"));
        assert_eq!(record.excluded[0], (Rule::Lifecycle, 1));
    }

    #[test]
    fn context_limit_excludes_only_the_small_replica() {
        let mut small = ready_view("gpu01", 0);
        small.state.limits.max_context_tokens = Some(100_000);
        let mut big = ready_view("gpu01", 1);
        big.state.limits.max_context_tokens = Some(1_000_000);
        let snap = snap_with(vec![small, big]);
        let mut input = base_input();
        input.context_tokens = Some(150_000);
        for seed in 0..8 {
            let mut rng = StdRng::seed_from_u64(seed);
            let (slot_, record, _) =
                placed(placer().place(&input, &snap, &HashMap::new(), &mut rng));
            assert_eq!(slot_, slot("gpu01", 1));
            assert_eq!(record.excluded[3], (Rule::Context, 1));
        }
    }

    #[test]
    fn two_replicas_on_one_host_are_scored_independently() {
        // Same host, one busy replica and one idle: the idle one wins every
        // time, and pending load on one slot never leaks onto its sibling.
        let mut busy = ready_view("gpu01", 0);
        busy.state.load.running = Some(30);
        busy.state.load.prefill_backlog_tokens = Some(40_000);
        let idle = ready_view("gpu01", 1);
        let snap = snap_with(vec![busy, idle]);
        for seed in 0..16 {
            let mut rng = StdRng::seed_from_u64(seed);
            let (slot_, record, _) =
                placed(placer().place(&base_input(), &snap, &HashMap::new(), &mut rng));
            assert_eq!(slot_, slot("gpu01", 1), "seed {seed}");
            assert_eq!(record.replica, Some(1));
            assert_eq!(record.slot.as_deref(), Some("gpu01#1"));
            assert_eq!(record.eligible, 2);
        }

        // Now load replica 1 through other nodes' routed counts only: it becomes
        // the worse slot, and replica 0 is picked.
        let mut snap = snap_with(vec![ready_view("gpu01", 0), ready_view("gpu01", 1)]);
        snap.routed.insert(
            slot("gpu01", 1),
            RoutedCounts {
                req: 4,
                tok: 60_000,
                since_ms: NOW - 1_000,
            },
        );
        let mut rng = StdRng::seed_from_u64(3);
        let (slot_, record, _) =
            placed(placer().place(&base_input(), &snap, &HashMap::new(), &mut rng));
        assert_eq!(slot_, slot("gpu01", 0));
        assert_eq!(record.pending_req, 0);
    }

    #[test]
    fn hrw_home_moves_only_for_removed_replica() {
        let all: Vec<SlotId> = vec![
            slot("gpu01", 0),
            slot("gpu01", 1),
            slot("gpu02", 0),
            slot("gpu02", 1),
        ];
        let removed = slot("gpu01", 1);
        let full = snap_with(all.iter().map(|s| ready_view(&s.host, s.replica)).collect());
        let reduced = snap_with(
            all.iter()
                .filter(|s| **s != removed)
                .map(|s| ready_view(&s.host, s.replica))
                .collect(),
        );
        let mut moved = 0;
        for k in 0u8..100 {
            let input = keyed(AffinityKey::from_bytes([k; 16]));
            let mut rng = StdRng::seed_from_u64(1);
            let (before, rec, _) = placed(placer().place(&input, &full, &HashMap::new(), &mut rng));
            assert_eq!(rec.selection, Some("home"));
            let (after, _, _) = placed(placer().place(&input, &reduced, &HashMap::new(), &mut rng));
            if before == removed {
                moved += 1;
                assert_ne!(after, removed);
            } else {
                assert_eq!(after, before, "key {k}: only the removed slot's keys move");
            }
        }
        assert!(
            moved > 0,
            "some keys should have had the removed slot as home"
        );
    }

    #[test]
    fn pin_to_vanished_slot_falls_through_to_hrw() {
        let secret = [4u8; 32];
        let slots = vec![slot("gpu01", 0), slot("gpu02", 0)];
        let key = find_key_with_home(&slots, &slot("gpu02", 0));
        let pid = pin_id(Tier::Base, &key, &secret);
        let mut snap = snap_with(vec![ready_view("gpu01", 0), ready_view("gpu02", 0)]);
        // The pin names gpu01#3, a replica index gone from gpu01's frame.
        std::sync::Arc::make_mut(&mut snap.pins).insert(
            *pid.as_bytes(),
            slot("gpu01", 3),
            NOW - 1_000,
        );
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, pin_write) = placed(Placer::new(secret, Tier::Base).place(
            &keyed(key),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_eq!(chosen, slot("gpu02", 0));
        assert_eq!(record.selection, Some("home"));
        assert_eq!(record.pinned.as_deref(), Some("gpu01#3"));
        let (_, rewritten) = pin_write.expect("a dead pin is rewritten at home");
        assert_eq!(rewritten, slot("gpu02", 0));
    }

    #[test]
    fn two_placers_agree() {
        let input = keyed(AffinityKey::from_bytes([3u8; 16]));
        let snap = snap_with(vec![ready_view("gpu01", 0), ready_view("gpu02", 0)]);

        let a = Placer::new([9u8; 32], Tier::Base);
        let b = Placer::new([9u8; 32], Tier::Base);
        let mut rng_a = StdRng::seed_from_u64(1);
        let mut rng_b = StdRng::seed_from_u64(2);
        let (slot_a, _, _) = placed(a.place(&input, &snap, &HashMap::new(), &mut rng_a));
        let (slot_b, _, _) = placed(b.place(&input, &snap, &HashMap::new(), &mut rng_b));
        assert_eq!(slot_a, slot_b);
    }

    #[test]
    fn two_placers_agree_after_pin() {
        let slots = vec![slot("gpu01", 0), slot("gpu02", 0)];
        let key = find_key_with_home(&slots, &slot("gpu01", 0));

        // gpu01#0 (home) is overloaded; gpu02#0 is the spill target.
        let hot_snap = || {
            let mut hot = ready_view("gpu01", 0);
            hot.state.load.running = Some(1000);
            snap_with(vec![hot, ready_view("gpu02", 0)])
        };
        let secret = [5u8; 32];
        let input = keyed(key);

        let mut rng = StdRng::seed_from_u64(1);
        let (slot_a, record_a, pin_write) = placed(Placer::new(secret, Tier::Base).place(
            &input,
            &hot_snap(),
            &HashMap::new(),
            &mut rng,
        ));
        assert_eq!(record_a.selection, Some("spill"));
        assert_eq!(record_a.home.as_deref(), Some("gpu01#0"));

        // Node A writes the pin into a fresh snapshot as node B would read it.
        let (pin_id_val, pinned_slot) = pin_write.expect("spill away from home must write a pin");
        assert_eq!(pinned_slot, slot_a);
        let mut snap_b = hot_snap();
        std::sync::Arc::make_mut(&mut snap_b.pins).insert(*pin_id_val.as_bytes(), pinned_slot, NOW);

        let mut rng_b = StdRng::seed_from_u64(2);
        let (slot_b, record_b, _) = placed(Placer::new(secret, Tier::Base).place(
            &input,
            &snap_b,
            &HashMap::new(),
            &mut rng_b,
        ));
        assert_eq!(slot_b, slot_a);
        assert_eq!(record_b.selection, Some("pinned"));
    }

    #[test]
    fn best_of_two_is_deterministic_across_replica_insertion_order() {
        // No affinity key, so `select` takes the keyless BestOfTwo path,
        // which samples indices against the score order. Two snapshots built
        // from the same replicas in reverse insertion order must still agree
        // for the same rng seed, because `place` sorts candidates by slot.
        let views_a = vec![
            ready_view("gpu01", 0),
            ready_view("gpu01", 1),
            ready_view("gpu02", 0),
            ready_view("gpu03", 0),
        ];
        let mut views_b = views_a.clone();
        views_b.reverse();
        let snap_a = snap_with(views_a);
        let snap_b = snap_with(views_b);

        let mut rng_a = StdRng::seed_from_u64(99);
        let mut rng_b = StdRng::seed_from_u64(99);
        let (a, _, _) = placed(placer().place(&base_input(), &snap_a, &HashMap::new(), &mut rng_a));
        let (b, _, _) = placed(placer().place(&base_input(), &snap_b, &HashMap::new(), &mut rng_b));
        assert_eq!(a, b);
    }

    #[test]
    fn pinned_refreshes_after_half_ttl() {
        let key_bytes = [6u8; 16];
        let secret = [2u8; 32];
        let views = vec![ready_view("gpu01", 0), ready_view("gpu02", 0)];
        let input = keyed(AffinityKey::from_bytes(key_bytes));
        let pid = pin_id(Tier::Base, &AffinityKey::from_bytes(key_bytes), &secret);
        let p = Placer::new(secret, Tier::Base);

        // Pin inserted just over half the TTL ago: must refresh.
        let mut snap_old = snap_with(views.clone());
        std::sync::Arc::make_mut(&mut snap_old.pins).insert(
            *pid.as_bytes(),
            slot("gpu01", 0),
            NOW - (PIN_TTL_MS / 2 + 1),
        );
        let mut rng = StdRng::seed_from_u64(1);
        let (_, record, pin_write) = placed(p.place(&input, &snap_old, &HashMap::new(), &mut rng));
        assert_eq!(record.selection, Some("pinned"));
        assert!(pin_write.is_some(), "half-TTL-old pin should refresh");

        // Pin inserted recently: no refresh needed.
        let mut snap_fresh = snap_with(views);
        std::sync::Arc::make_mut(&mut snap_fresh.pins).insert(
            *pid.as_bytes(),
            slot("gpu01", 0),
            NOW - 1_000,
        );
        let mut rng2 = StdRng::seed_from_u64(1);
        let (_, record, pin_write) =
            placed(p.place(&input, &snap_fresh, &HashMap::new(), &mut rng2));
        assert_eq!(record.selection, Some("pinned"));
        assert!(pin_write.is_none(), "recent pin should not refresh");
    }

    /// Fails if `s` contains `key_bytes` or `pid` in any of the encodings a
    /// careless `Debug`/`Display` would produce.
    fn assert_no_key_material(s: &str, key_bytes: [u8; 16], pid: &PinId) {
        assert!(
            !s.contains(&pid.to_hex()),
            "output leaked the pin id's hex encoding: {s}"
        );
        assert!(
            !s.contains(&hex::encode(key_bytes)),
            "output leaked the affinity key's hex encoding: {s}"
        );
        assert!(
            !s.contains(&format!("{key_bytes:?}")),
            "output leaked the affinity key's raw (decimal) bytes: {s}"
        );
        assert!(
            !s.contains(&format!("{:?}", pid.as_bytes())),
            "output leaked the pin id's raw (decimal) bytes: {s}"
        );
        // Belt-and-suspenders: a leaked pin id or raw key byte dump would
        // also show up as a run of 32+ hex characters.
        let mut run = 0usize;
        for c in s.chars() {
            if c.is_ascii_hexdigit() {
                run += 1;
                assert!(run < 32, "found a 32+ hex-char run in output: {s}");
            } else {
                run = 0;
            }
        }
    }

    #[test]
    fn record_never_contains_key_material() {
        let key_bytes = [0xABu8; 16];
        let secret = [0xCDu8; 32];
        let pid = pin_id(Tier::Base, &AffinityKey::from_bytes(key_bytes), &secret);
        let input = keyed(AffinityKey::from_bytes(key_bytes));

        // Give the request a live pin so the `Pinned` path (and its
        // `pin_lookup`) actually runs, instead of trivially passing on a
        // Legacy or keyless decision that never touches key material.
        let mut snap = snap_with(vec![ready_view("gpu01", 0), ready_view("gpu02", 0)]);
        std::sync::Arc::make_mut(&mut snap.pins).insert(
            *pid.as_bytes(),
            slot("gpu01", 0),
            NOW - 1_000,
        );

        let mut rng = StdRng::seed_from_u64(1);
        let (_, record, _) =
            placed(Placer::new(secret, Tier::Base).place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(record.selection, Some("pinned"));
        assert_no_key_material(&format!("{record:?}"), key_bytes, &pid);

        // The request input's own Debug redacts the key.
        assert_no_key_material(&format!("{input:?}"), key_bytes, &pid);

        // A Legacy decision's record carries none either.
        snap.disabled = true;
        let (_, record) = legacy_reason(Placer::new(secret, Tier::Base).place(
            &input,
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_no_key_material(&format!("{record:?}"), key_bytes, &pid);
    }

    // --- Heavy lane (Rule::Lane) and the route policy label ---

    fn heavy(prompt_tokens: u64) -> PlaceInput {
        let mut input = base_input();
        input.heavy = true;
        input.prompt_tokens = prompt_tokens;
        input
    }

    fn with_backlog(host: &str, replica: u32, backlog: u64) -> ReplicaView {
        let mut v = ready_view(host, replica);
        v.state.load.prefill_backlog_tokens = Some(backlog);
        v
    }

    /// 8 slots: gpu01..gpu04, replicas 0 and 1.
    fn eight_slots() -> Vec<ReplicaView> {
        (1..=4)
            .flat_map(|h| (0..2).map(move |r| ready_view(&format!("gpu0{h}"), r)))
            .collect()
    }

    fn refused_record(d: Decision) -> DecisionRecord {
        match d {
            Decision::Refused { record } => record,
            Decision::Place { record, .. } => panic!("expected Refused, placed {:?}", record.slot),
            Decision::Legacy { reason, .. } => panic!("expected Refused, got Legacy({reason:?})"),
        }
    }

    #[test]
    fn eight_heavy_on_eight_base_replicas() {
        let snap = snap_with(eight_slots());
        let prompt = 120_000;
        let mut mine: HashMap<SlotId, Pending> = HashMap::new();
        let mut strategies = Vec::new();
        for seed in 0..8 {
            let mut rng = StdRng::seed_from_u64(seed);
            match placer().place(&heavy(prompt), &snap, &mine, &mut rng) {
                Decision::Place { slot, record, .. } => {
                    let p = mine.entry(slot).or_default();
                    p.req += 1;
                    p.tok += prompt;
                    strategies.push(record.strategy.unwrap());
                }
                Decision::Refused { record } => {
                    assert_eq!(record.excluded[4], (Rule::Lane, 8));
                    assert_eq!(record.reason, Some("lane_full"));
                    assert_eq!((record.lane_size, record.lane_cap), (2, 2));
                    strategies.push(record.strategy.unwrap());
                }
                Decision::Legacy { reason, .. } => panic!("unexpected Legacy({reason:?})"),
            }
        }
        // Two new members, each joined once more until the next prompt would
        // take it over HEAVY_BACKLOG_CAP (3 x 120K > 300K), then refusals.
        assert_eq!(
            strategies,
            [
                "heavy_lane_admit",
                "heavy_lane_admit",
                "heavy_lane_join",
                "heavy_lane_join",
                "refuse",
                "refuse",
                "refuse",
                "refuse"
            ]
        );
        let members: Vec<&SlotId> = mine
            .iter()
            .filter(|(_, p)| p.tok >= crate::consts::LANE_LOAD_TOKENS)
            .map(|(s, _)| s)
            .collect();
        assert_eq!(members.len(), 2, "ceil(8 * 0.25) lane members");

        // Short requests land only on the 6 clean replicas.
        let mut short = base_input();
        short.prompt_tokens = 500;
        for seed in 0..64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let (chosen, record, _) = placed(placer().place(&short, &snap, &mine, &mut rng));
            assert!(
                !members.contains(&&chosen),
                "seed {seed}: short landed on {chosen:?}"
            );
            assert_eq!(record.strategy, Some("short_clean"));
            assert_eq!(record.eligible, 6);
            assert_eq!(record.excluded[4], (Rule::Lane, 2));
        }
    }

    #[test]
    fn misestimated_backlog_makes_member() {
        // A 70K backlog put there by short-class requests (an underestimate,
        // or many of them) makes gpu01#0 a lane member all the same.
        let snap = snap_with(vec![
            with_backlog("gpu01", 0, 70_000),
            ready_view("gpu01", 1),
            ready_view("gpu02", 0),
            ready_view("gpu02", 1),
        ]);
        for seed in 0..32 {
            let mut rng = StdRng::seed_from_u64(seed);
            let (chosen, record, _) =
                placed(placer().place(&base_input(), &snap, &HashMap::new(), &mut rng));
            assert_ne!(chosen, slot("gpu01", 0), "seed {seed}");
            assert_eq!(record.strategy, Some("short_clean"));
        }
        // It also fills the 4-replica lane (cap 1), so a heavy request joins it.
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, _) =
            placed(placer().place(&heavy(100_000), &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, slot("gpu01", 0));
        assert_eq!(record.strategy, Some("heavy_lane_join"));
    }

    #[test]
    fn no_clean_replica_gives_short_overflow() {
        let snap = snap_with(vec![
            with_backlog("gpu01", 0, 70_000),
            with_backlog("gpu01", 1, 400_000),
        ]);
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, _) =
            placed(placer().place(&base_input(), &snap, &HashMap::new(), &mut rng));
        assert_eq!(
            chosen,
            slot("gpu01", 0),
            "the lighter member still wins on score"
        );
        assert_eq!(record.strategy, Some("short_overflow"));
        assert_eq!(record.excluded[4], (Rule::Lane, 0));
    }

    #[test]
    fn long_tier_refuses_heavy_when_all_over_cap() {
        let long = Placer::new([1u8; 32], Tier::Long);
        let snap = snap_with(vec![
            with_backlog("long01", 0, 550_000),
            with_backlog("long01", 1, 550_000),
        ]);
        let mut rng = StdRng::seed_from_u64(1);
        let record = refused_record(long.place(&heavy(100_000), &snap, &HashMap::new(), &mut rng));
        assert_eq!(record.outcome, "refused");
        assert_eq!(record.strategy, Some("refuse"));
        assert_eq!(record.excluded[4], (Rule::Lane, 2));
        assert_eq!(record.reason, Some("long_full"));

        // One replica with room: placed there, and the long tier ignores
        // the base lane cap (both are "members").
        let snap = snap_with(vec![
            with_backlog("long01", 0, 550_000),
            with_backlog("long01", 1, 400_000),
        ]);
        let (chosen, record, _) =
            placed(long.place(&heavy(100_000), &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, slot("long01", 1));
        assert_eq!(record.strategy, Some("heavy_long"));
    }

    #[test]
    fn heavy_with_all_replicas_stale_is_legacy_not_refused() {
        let views = eight_slots()
            .into_iter()
            .map(|mut v| {
                v.state.engine_sampled_at_ms = Some(NOW - FRESH_MAX_MS - 1);
                v
            })
            .collect();
        let snap = snap_with(views);
        let mut rng = StdRng::seed_from_u64(1);
        let (reason, record) =
            legacy_reason(placer().place(&heavy(200_000), &snap, &HashMap::new(), &mut rng));
        assert_eq!(reason, LegacyReason::NoneEligible);
        assert_eq!(record.excluded[1], (Rule::Freshness, 8));
        assert_eq!(record.excluded[4], (Rule::Lane, 0));
    }

    #[test]
    fn heavy_with_no_views_is_legacy_not_refused() {
        let mut rng = StdRng::seed_from_u64(1);
        for tier in [Tier::Base, Tier::Long] {
            let p = Placer::new([1u8; 32], tier);
            let (reason, _) = legacy_reason(p.place(
                &heavy(200_000),
                &snap_with(vec![]),
                &HashMap::new(),
                &mut rng,
            ));
            assert_eq!(reason, LegacyReason::NoState);
        }
    }

    #[test]
    fn short_request_on_long_tier_is_never_refused() {
        let long = Placer::new([1u8; 32], Tier::Long);
        let snap = snap_with(vec![
            with_backlog("long01", 0, 10_000_000),
            with_backlog("long01", 1, 10_000_000),
        ]);
        let mut rng = StdRng::seed_from_u64(1);
        let (_, record, _) = placed(long.place(&base_input(), &snap, &HashMap::new(), &mut rng));
        assert_eq!(record.strategy, Some("short_overflow"));
    }

    /// 8 slots (lane cap 2) where `gpu02#1` is the only lane member, with a
    /// 100K backlog, and the key's HRW home is `gpu01#0`. The lane has room,
    /// so every idle slot is admitted too, and the pinned member's score
    /// (~6.25) is far outside the affinity bound of the idle best (0).
    fn pinned_member_with_room(secret: [u8; 32], backlog: u64) -> (Snapshot, AffinityKey, SlotId) {
        let pinned = slot("gpu02", 1);
        let mut views = eight_slots();
        views[3].state.load.prefill_backlog_tokens = Some(backlog); // gpu02#1
        let all: Vec<SlotId> = views.iter().map(|v| v.slot.clone()).collect();
        let key = find_key_with_home(&all, &slot("gpu01", 0));
        let mut snap = snap_with(views);
        let pid = pin_id(Tier::Base, &key, &secret);
        std::sync::Arc::make_mut(&mut snap.pins).insert(
            *pid.as_bytes(),
            pinned.clone(),
            NOW - 1_000,
        );
        (snap, key, pinned)
    }

    #[test]
    fn pinned_heavy_conversation_returns_to_pinned_member() {
        let secret = [8u8; 32];
        let (snap, key, pinned) = pinned_member_with_room(secret, 100_000);
        let mut input = keyed(key);
        input.heavy = true;
        input.prompt_tokens = 100_000;
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, pin_write) =
            placed(Placer::new(secret, Tier::Base).place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, pinned);
        assert_eq!(record.selection, Some("pinned"));
        assert_eq!(record.strategy, Some("heavy_lane_join"));
        assert_eq!((record.lane_size, record.lane_cap), (1, 2));
        assert_eq!(
            record.eligible, 8,
            "the lane had room: idle slots were admitted"
        );
        let (chosen_score, best) = (record.chosen_score.unwrap(), record.best_score.unwrap());
        assert!(
            chosen_score > best + 1.0,
            "the pin must be outside the score bound here: {chosen_score} vs {best}"
        );
        let (_, rewritten) = pin_write.expect("a heavy placement always refreshes its pin");
        assert_eq!(rewritten, pinned);
    }

    #[test]
    fn pinned_short_conversation_still_spills_outside_bound() {
        // Same state, short request: the bound applies, so the pinned slot
        // (out of bound, and a lane member) is left for the HRW home.
        let secret = [8u8; 32];
        let (snap, key, pinned) = pinned_member_with_room(secret, 100_000);
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, pin_write) = placed(Placer::new(secret, Tier::Base).place(
            &keyed(key.clone()),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_ne!(chosen, pinned);
        assert_eq!(chosen, slot("gpu01", 0));
        assert_eq!(record.selection, Some("home"));
        assert!(pin_write.is_some(), "the moved pin is rewritten");

        // Even with the lane member admitted (no clean replica), the short
        // pin is still bounded: a clean-but-loaded fleet shows it directly.
        let mut views = eight_slots();
        for v in &mut views {
            v.state.load.prefill_backlog_tokens = Some(LANE_LOAD_TOKENS);
        }
        views[3].state.load.prefill_backlog_tokens = Some(200_000);
        let mut snap = snap_with(views);
        let pid = pin_id(Tier::Base, &key, &secret);
        std::sync::Arc::make_mut(&mut snap.pins).insert(
            *pid.as_bytes(),
            pinned.clone(),
            NOW - 1_000,
        );
        let (chosen, record, _) = placed(Placer::new(secret, Tier::Base).place(
            &keyed(key),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_ne!(chosen, pinned);
        assert_eq!(record.strategy, Some("short_overflow"));
    }

    #[test]
    fn heavy_home_placement_writes_pin() {
        let secret = [8u8; 32];
        let slots: Vec<SlotId> = eight_slots().iter().map(|v| v.slot.clone()).collect();
        let home = slot("gpu03", 1);
        let key = find_key_with_home(&slots, &home);
        let snap = snap_with(eight_slots());
        let mut input = keyed(key.clone());
        input.heavy = true;
        input.prompt_tokens = 150_000;
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, pin_write) =
            placed(Placer::new(secret, Tier::Base).place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, home);
        assert_eq!(record.selection, Some("home"));
        let (pid, pinned) = pin_write.expect("a keyed heavy Home placement writes a pin");
        assert_eq!(pinned, home);
        assert_eq!(pid.as_bytes(), pin_id(Tier::Base, &key, &secret).as_bytes());

        // A fresh pin on the heavy Pinned path is refreshed too.
        let mut snap = snap_with(eight_slots());
        std::sync::Arc::make_mut(&mut snap.pins).insert(*pid.as_bytes(), home.clone(), NOW - 1_000);
        let (_, record, pin_write) =
            placed(Placer::new(secret, Tier::Base).place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(record.selection, Some("pinned"));
        assert!(pin_write.is_some());
    }

    #[test]
    fn short_home_placement_writes_no_pin() {
        // Today's short-request rule: Home with no prior pin writes nothing.
        let slots: Vec<SlotId> = eight_slots().iter().map(|v| v.slot.clone()).collect();
        let home = slot("gpu03", 1);
        let key = find_key_with_home(&slots, &home);
        let snap = snap_with(eight_slots());
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, pin_write) = placed(Placer::new([8u8; 32], Tier::Base).place(
            &keyed(key),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_eq!(chosen, home);
        assert_eq!(record.selection, Some("home"));
        assert!(pin_write.is_none());
    }

    #[test]
    fn oversized_heavy_prompt_is_placed_on_an_idle_long_replica() {
        let long = Placer::new([1u8; 32], Tier::Long);
        let snap = snap_with(vec![
            with_backlog("long01", 0, 100_000),
            ready_view("long01", 1),
        ]);
        let mut input = heavy(LONG_BACKLOG_CAP + 100_000);
        input.context_tokens = Some(LONG_BACKLOG_CAP + 120_000);
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, _) = placed(long.place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, slot("long01", 1));
        assert_eq!(record.strategy, Some("heavy_long"));
        assert_eq!(record.excluded[4], (Rule::Lane, 1));
    }

    #[test]
    fn heavy_pin_over_backlog_cap_moves() {
        // The pinned member's load plus the prompt exceeds HEAVY_BACKLOG_CAP,
        // so stage 2 excludes it and the pin can't hold: it spills to a new
        // lane member (the lane has room) and the pin is rewritten.
        let secret = [8u8; 32];
        let (snap, key, pinned) = pinned_member_with_room(secret, HEAVY_BACKLOG_CAP - 50_000);
        let mut input = keyed(key);
        input.heavy = true;
        input.prompt_tokens = 100_000;
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, pin_write) =
            placed(Placer::new(secret, Tier::Base).place(&input, &snap, &HashMap::new(), &mut rng));
        assert_ne!(chosen, pinned);
        assert_eq!(record.excluded[4], (Rule::Lane, 1));
        assert_eq!(record.strategy, Some("heavy_lane_admit"));
        assert_eq!(record.selection, Some("home"));
        assert_eq!(record.pinned.as_deref(), Some("gpu02#1"));
        let (_, rewritten) = pin_write.expect("the moved pin is rewritten");
        assert_eq!(rewritten, chosen);
    }

    #[test]
    fn pinned_heavy_conversation_on_drained_replica_readmits_it() {
        // The pinned slot drained out of the lane; another slot is a member,
        // and the 8-slot lane (cap 2) has room, so the pinned slot is
        // re-admitted as a new member and the pin holds.
        let pinned = slot("gpu03", 0);
        let key = AffinityKey::from_bytes([21u8; 16]);
        let secret = [8u8; 32];
        let mut views = eight_slots();
        views[0].state.load.prefill_backlog_tokens = Some(100_000); // gpu01#0
        let mut snap = snap_with(views);
        let pid = pin_id(Tier::Base, &key, &secret);
        std::sync::Arc::make_mut(&mut snap.pins).insert(
            *pid.as_bytes(),
            pinned.clone(),
            NOW - 1_000,
        );

        let mut input = keyed(key);
        input.heavy = true;
        input.prompt_tokens = 100_000;
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, _) =
            placed(Placer::new(secret, Tier::Base).place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, pinned);
        assert_eq!(record.selection, Some("pinned"));
        assert_eq!(record.strategy, Some("heavy_lane_admit"));
    }

    // --- Refused, tier-keyed pins and record fields ---

    /// A heavy request on a saturated 4-slot base Fleet: every replica is a
    /// member at the backlog cap, so a usable snapshot refuses it.
    fn saturated_base() -> Snapshot {
        snap_with(
            (0..4)
                .map(|r| with_backlog("gpu01", r, HEAVY_BACKLOG_CAP))
                .collect(),
        )
    }

    #[test]
    fn refused_record_names_tier_class_and_lane() {
        let mut input = heavy(100_000);
        input.context_tokens = Some(120_000);
        input.priority = -1;
        let mut rng = StdRng::seed_from_u64(1);
        let record =
            refused_record(placer().place(&input, &saturated_base(), &HashMap::new(), &mut rng));
        assert_eq!(record.outcome, "refused");
        assert_eq!(record.reason, Some("lane_full"));
        assert_eq!(record.strategy, Some("refuse"));
        assert_eq!(record.tier, Tier::Base);
        assert_eq!(record.class, Class::Heavy);
        assert_eq!(record.priority_band, "neg");
        assert_eq!((record.lane_size, record.lane_cap), (4, 1));
        assert_eq!(record.prompt_tokens, 100_000);
        assert_eq!(record.context_tokens, Some(120_000));
        assert_eq!(record.eligible, 0);
        assert_eq!(record.slot, None);

        let long = Placer::new([1u8; 32], Tier::Long);
        let snap = snap_with(vec![with_backlog("long01", 0, LONG_BACKLOG_CAP)]);
        let record = refused_record(long.place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(record.reason, Some("long_full"));
        assert_eq!(record.tier, Tier::Long);
    }

    #[test]
    fn placed_record_carries_request_numbers() {
        let mut input = base_input();
        input.prompt_tokens = 4_000;
        input.context_tokens = Some(12_000);
        input.priority = 3;
        let snap = snap_with(eight_slots());
        let mut rng = StdRng::seed_from_u64(1);
        let (_, record, _) = placed(placer().place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(record.outcome, "place");
        assert_eq!(record.tier, Tier::Base);
        assert_eq!(record.class, Class::Short);
        assert_eq!(record.priority_band, "high");
        assert_eq!((record.lane_size, record.lane_cap), (0, 2));
        assert_eq!(record.prompt_tokens, 4_000);
        assert_eq!(record.context_tokens, Some(12_000));
    }

    #[test]
    fn refused_never_without_state() {
        let stale = || {
            let mut s = saturated_base();
            s.built_ms = NOW - FRESH_MAX_MS - 1;
            s
        };
        let not_ready = || {
            let mut s = saturated_base();
            for v in &mut s.replicas {
                v.state.lifecycle_state = crate::frame::Lifecycle::Draining;
            }
            s
        };
        let cases: [(Snapshot, LegacyReason); 3] = [
            (snap_with(vec![]), LegacyReason::NoState),
            (stale(), LegacyReason::Stale),
            (not_ready(), LegacyReason::NoneEligible),
        ];
        for (snap, want) in cases {
            for tier in [Tier::Base, Tier::Long] {
                let mut rng = StdRng::seed_from_u64(1);
                let d = Placer::new([1u8; 32], tier).place(
                    &heavy(250_000),
                    &snap,
                    &HashMap::new(),
                    &mut rng,
                );
                let (reason, record) = legacy_reason(d);
                assert_eq!(reason, want, "{tier:?}");
                assert_eq!(record.outcome, "legacy");
                assert_eq!(record.strategy, None);
            }
        }
    }

    #[test]
    fn refused_never_when_disabled() {
        let mut snap = saturated_base();
        let mut rng = StdRng::seed_from_u64(1);
        refused_record(placer().place(&heavy(100_000), &snap, &HashMap::new(), &mut rng));

        snap.disabled = true;
        let (reason, record) =
            legacy_reason(placer().place(&heavy(100_000), &snap, &HashMap::new(), &mut rng));
        assert_eq!(reason, LegacyReason::Disabled);
        assert_eq!(record.tier, Tier::Base);
        assert_eq!(record.class, Class::Heavy);
    }

    #[test]
    fn priority_band_tags() {
        assert_eq!(PriorityBand::of(i32::MIN).as_str(), "neg");
        assert_eq!(PriorityBand::of(-1).as_str(), "neg");
        assert_eq!(PriorityBand::of(0).as_str(), "normal");
        assert_eq!(PriorityBand::of(1).as_str(), "high");
        assert_eq!(PriorityBand::of(i32::MAX).as_str(), "high");

        let mut input = base_input();
        input.priority = -7;
        let mut rng = StdRng::seed_from_u64(1);
        let snap = snap_with(vec![ready_view("gpu01", 0)]);
        let (_, record, _) = placed(placer().place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(record.priority_band, "neg");
        // Legacy records carry the band too.
        let (_, record) =
            legacy_reason(placer().place(&input, &snap_with(vec![]), &HashMap::new(), &mut rng));
        assert_eq!(record.priority_band, "neg");
    }

    #[test]
    fn pins_on_different_tiers_never_collide() {
        let key = AffinityKey::from_bytes([33u8; 16]);
        let secret = [6u8; 32];
        let base_pid = pin_id(Tier::Base, &key, &secret);
        let long_pid = pin_id(Tier::Long, &key, &secret);
        assert_ne!(base_pid.as_bytes(), long_pid.as_bytes());

        // A base-tier pin is invisible to the long placer, even when the
        // pinned slot is one the long placer can see.
        let slots = vec![slot("long01", 0), slot("long01", 1)];
        let home = slot("long01", 0);
        let key = find_key_with_home(&slots, &home);
        let mut snap = snap_with(vec![ready_view("long01", 0), ready_view("long01", 1)]);
        std::sync::Arc::make_mut(&mut snap.pins).insert(
            *pin_id(Tier::Base, &key, &secret).as_bytes(),
            slot("long01", 1),
            NOW - 1_000,
        );
        let mut input = keyed(key.clone());
        input.heavy = true;
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, pin_write) =
            placed(Placer::new(secret, Tier::Long).place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, home);
        assert_eq!(record.selection, Some("home"));
        assert_eq!(record.pinned, None);
        // The long placer writes its own pin (heavy placements always pin),
        // under its own tier's id: never the base pin's.
        let (long_pid, _) = pin_write.expect("heavy placements pin");
        assert_eq!(
            long_pid.as_bytes(),
            pin_id(Tier::Long, &key, &secret).as_bytes()
        );
        assert_ne!(
            long_pid.as_bytes(),
            pin_id(Tier::Base, &key, &secret).as_bytes()
        );

        // The base placer does honor it.
        let (chosen, record, _) = placed(Placer::new(secret, Tier::Base).place(
            &keyed(key),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_eq!(chosen, slot("long01", 1));
        assert_eq!(record.selection, Some("pinned"));
    }

    #[test]
    fn refused_record_never_contains_key_material() {
        let key_bytes = [0xABu8; 16];
        let secret = [0xCDu8; 32];
        let pid = pin_id(Tier::Base, &AffinityKey::from_bytes(key_bytes), &secret);
        let mut snap = saturated_base();
        std::sync::Arc::make_mut(&mut snap.pins).insert(
            *pid.as_bytes(),
            slot("gpu01", 0),
            NOW - 1_000,
        );
        let mut input = keyed(AffinityKey::from_bytes(key_bytes));
        input.heavy = true;
        input.prompt_tokens = 100_000;
        let mut rng = StdRng::seed_from_u64(1);
        let record = refused_record(Placer::new(secret, Tier::Base).place(
            &input,
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_no_key_material(&format!("{record:?}"), key_bytes, &pid);
    }

    use proptest::prelude::*;

    fn lifecycle_strategy() -> impl Strategy<Value = crate::frame::Lifecycle> {
        prop_oneof![
            Just(crate::frame::Lifecycle::Ready),
            Just(crate::frame::Lifecycle::Warming),
            Just(crate::frame::Lifecycle::Degraded),
            Just(crate::frame::Lifecycle::Draining),
            Just(crate::frame::Lifecycle::Unhealthy),
        ]
    }

    /// `(lifecycle, is_fresh, running)` for one randomly generated replica.
    fn replica_spec_strategy() -> impl Strategy<Value = (crate::frame::Lifecycle, bool, u32)> {
        (lifecycle_strategy(), proptest::bool::ANY, 0u32..50)
    }

    fn view_from_spec(
        host: &str,
        replica: u32,
        spec: (crate::frame::Lifecycle, bool, u32),
    ) -> ReplicaView {
        let (lifecycle, fresh, running) = spec;
        let mut v = ready_view(host, replica);
        v.state.lifecycle_state = lifecycle;
        v.state.engine_sampled_at_ms = if fresh {
            Some(NOW)
        } else {
            Some(NOW - FRESH_MAX_MS - 1)
        };
        v.state.load.running = Some(running);
        v
    }

    proptest! {
        /// Small random snapshots with an independently random lifecycle,
        /// freshness, and load per replica (not just per run), plus an
        /// optional affinity key, follow pin, and `mine` entry. Whenever the
        /// placer actually places, the chosen slot must be eligible.
        #[test]
        fn placed_slot_is_always_eligible(
            specs in proptest::collection::vec(replica_spec_strategy(), 1..=12),
            has_affinity in proptest::bool::ANY,
            has_pin in proptest::bool::ANY,
            has_mine in proptest::bool::ANY,
        ) {
            let mut views = Vec::new();
            for (i, spec) in specs.into_iter().enumerate() {
                views.push(view_from_spec(&format!("gpu{}", i / 2), (i % 2) as u32, spec));
            }

            let mut input = base_input();
            input.model = COVERED_MODELS[0].to_string();
            let key_bytes = [42u8; 16];
            if has_affinity {
                input.affinity = Some(AffinityKey::from_bytes(key_bytes));
                input.affinity_source = AffinitySource::Client;
            }

            let mut snap = snap_with(views);
            if has_pin && has_affinity {
                let pid = pin_id(Tier::Base, &AffinityKey::from_bytes(key_bytes), &[1u8; 32]);
                std::sync::Arc::make_mut(&mut snap.pins).insert(*pid.as_bytes(), slot("gpu0", 0), NOW - 1_000);
            }

            let mut mine = HashMap::new();
            if has_mine {
                mine.insert(slot("gpu0", 0), Pending { req: 2, tok: 500 });
            }

            let mut rng = StdRng::seed_from_u64(42);
            let decision = placer().place(&input, &snap, &mine, &mut rng);
            if let Decision::Place { slot, .. } = decision {
                let eligible = snap
                    .replicas
                    .iter()
                    .any(|v| v.slot == slot && first_exclusion(v, &input, input.now_ms).is_none());
                prop_assert!(eligible);
            }
        }

        /// Guards against `placed_slot_is_always_eligible` passing vacuously
        /// (e.g. if a bug made the placer stop ever choosing `Place`): a
        /// snapshot that always includes one guaranteed-eligible replica
        /// among the random ones must always yield `Decision::Place`.
        #[test]
        fn eligible_replica_forces_place(
            specs in proptest::collection::vec(replica_spec_strategy(), 0..=8),
        ) {
            let mut views = Vec::new();
            for (i, spec) in specs.into_iter().enumerate() {
                views.push(view_from_spec(&format!("gpuX{}", i / 2), (i % 2) as u32, spec));
            }
            views.push(ready_view("gpu-forced-eligible", 0));

            let mut input = base_input();
            input.model = COVERED_MODELS[0].to_string();
            let snap = snap_with(views);

            let mut rng = StdRng::seed_from_u64(7);
            let decision = placer().place(&input, &snap, &HashMap::new(), &mut rng);
            let placed = matches!(decision, Decision::Place { .. });
            prop_assert!(placed);
        }
    }

    #[test]
    fn routed_counts_are_per_slot() {
        let mut snap = snap_with(vec![ready_view("gpu01", 0)]);
        snap.routed.insert(
            slot("gpu01", 0),
            RoutedCounts {
                req: 5,
                tok: 100,
                // As the reader builds it: the window starts a second
                // before now, i.e. before the frame's reported_at_ms.
                since_ms: NOW - 1_000,
            },
        );
        let mut mine = HashMap::new();
        mine.insert(slot("gpu01", 0), Pending { req: 2, tok: 10 });
        let mut rng = StdRng::seed_from_u64(1);
        let (_, record, _) = placed(placer().place(&base_input(), &snap, &mine, &mut rng));
        assert_eq!(record.pending_req, 7);
    }

    #[test]
    fn other_nodes_routed_load_shifts_decision() {
        // Two identical idle slots. Another node routed load to gpu-a#0 in
        // the current window (this node's own ledger is empty), so it must
        // score worse and the placer must pick gpu-b#0 — for every rng seed.
        let mut snap = snap_with(vec![ready_view("gpu-a", 0), ready_view("gpu-b", 0)]);
        snap.routed.insert(
            slot("gpu-a", 0),
            RoutedCounts {
                req: 8,
                tok: 40_000,
                since_ms: NOW - 1_000,
            },
        );
        for seed in 0..32 {
            let mut rng = StdRng::seed_from_u64(seed);
            let (chosen, _, _) =
                placed(placer().place(&base_input(), &snap, &HashMap::new(), &mut rng));
            assert_eq!(chosen, slot("gpu-b", 0), "seed {seed}");
        }
    }
}
